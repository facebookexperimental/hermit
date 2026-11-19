---
schema: syscall-pages/v3
syscall: clone
nr_x86_64: 56
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clone&body=Discussion+for+the+%60clone%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclone.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clone%60%2C+or+in+what+the+page+says%3F%0A
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
  reviewed_at: 2026-11-19
  based_on: 403e350e9e82ee540529f097daccae6a60832d07
---

## In short

Natively, `clone` creates a task whose identifier and first moments are settled by host allocation and scheduling, so which task runs first can differ between otherwise identical runs. Hermit still has the host kernel create the task, but admits it through a registration rendezvous into a serialized deterministic schedule, so a repeated schedule repeats the creation order. Known gaps: 5.

## Determinism strategy

**What is nondeterministic natively.** `clone` creates a new task that shares address space, descriptor table, signal handlers, and namespaces with its creator exactly as its flags direct, starts on a caller-supplied stack with caller-supplied thread-local storage, and receives a host-assigned identifier, so the returned identifier, which of creator and child runs first, and every later interleaving of the two are settled by host allocation and scheduling.[^linux]

**What Hermit does.** Detcore determinizes `clone` by keeping creation in the host kernel while Detcore owns the child's admission to its serialized schedule: typed dispatch reaches one handler for the whole clone family, covering this call together with `clone3`, `fork`, and `vfork`.[^c1][^c2] For the sibling `clone3` form served by the same handler, a request to reuse an earlier task's identifier, an identifier request Detcore cannot read, or one from a nested pid namespace is refused by name before any flag answer in a fail-closed run, rather than left to the kernel alone; a run that allows unsupported operations answers such a call with the refusal's fallback error instead of ending.[^c23][^c28][^c35] Before anything is created, the flag combinations Linux refuses from the flags alone for this legacy form are answered with `EINVAL` in Detcore, so for those flag combinations a backend is asked only for a creation Linux would perform.[^c3][^c4] When the run chooses guest identifiers itself, the child's identifier is taken in the caller's turn and carried into the issued call, so with sequentialized threads the identifiers follow the guest's own order of calls; the first task of a new pid namespace this same call creates from Hermit's own namespace still gets a chosen identifier.[^c5][^c20] A child created in a pid namespace the guest made earlier, a namespace that cannot be read, a call a guest-installed seccomp filter could make fare otherwise in its rewritten form (or an inherited filter count Hermit cannot establish, or a kernel that does not report the filter count), a call made when no unused chosen identifier is left, or a call with no chosen-identifier form instead keeps a host-chosen identifier as made, which differs from run to run, unless the run requires every identifier to be chosen, in which case it ends before any task exists (G4).[^c20][^c25][^c26][^c31] Otherwise the kernel numbers the child, in the run's pid namespace or on the host as the run's identifier mode directs.[^c19] The call, in its own or its rewritten form below, is then injected, and creator and child meet in a registration rendezvous at a fixed, seed-derived, or schedule-replayed starting priority: the child blocks at its thread start until the scheduler grants its first slice, and if the child arrives before its creator has registered it, the child alone waits, since the creator's registration does not block beyond its registration reply, so neither side can run ahead outside the schedule and the child cannot finish before it is registered, and the scheduler in turn waits for the child to take its first turn.[^c6][^c7][^c8][^c22][^c32] A `vfork` creator with sequentialized threads additionally gives up its turn before the blocking creation, and its child registers itself, matching the kernel's rule that the creator waits for the child to exec or exit.[^c17][^c32] Under the in-guest signal-delivery gate, a `vfork` call with a default-fatal signal already deliverable at entry is not run, and a case with a caught signal ahead of it is refused by name, failing closed; this page names that refusal without further covering the gate.[^c34]

**Interface fidelity.** Sharing flags, the child stack, and thread-local storage are honored by the kernel, but the issued call is not always in the guest's own form: when Detcore chooses the identifier it rewrites the call to the sibling form naming that identifier, dropping the detached flag the kernel ignores on its own, clearing the exit signal beside thread or parent sharing where the kernel would replace it in any case, and expressing a new stack pointer as the top of a one-byte stack range ending where the guest asked the child to start, with thread-local storage carried through unchanged.[^c21][^linux] A stack pointer with no room below it or one beyond user memory has no such rewritten form and falls back as above to a host-chosen identifier or a refused run (G4).[^c21][^c20] An exit signal beyond the last signal number is different: beside `CLONE_THREAD` or `CLONE_PARENT`, Detcore itself answers `EINVAL`, taking no identifier and issuing no call.[^c24] Without those flags the call has no rewritten form and is issued as made in a run that allows host identifiers, where on hosts admitted for chosen identifiers, which are probed for exactly this early signal validation, the kernel answers `EINVAL` and creates no task, so no host-chosen identifier results; a run requiring every identifier to be chosen instead ends before the kernel sees the call (G5).[^c24][^c20][^linux] The untraced flag is cleared wherever the call can be issued without it so every child stays traced and schedulable, a change no guest can observe because no guest task has a tracer of its own; where a call with no chosen-identifier form cannot be issued without it, the run ends in any chosen-identifier run rather than letting an untraced task run outside the schedule, while the parent-tracing flag is carried unchanged and a detached flag outside the rewrite is left to the kernel, which ignores it on its own.[^c6][^c21][^c27][^c20] Namespace-creating flags likewise take effect in the kernel, and combinations Linux rejects, such as a new mount or user namespace together with shared filesystem state, a thread without shared signal handlers, a pidfd together with the detached flag, or, on this legacy form whose pidfd word is its parent-tid word, a pidfd together with a parent-tid store, fail with `EINVAL` before any task exists, while checks that depend on caller state are left to the kernel's own answer.[^c3][^c4] The tid-store flags are the kernel's writes; Detcore registers only a clear-tid address, and its scheduler then simulates that clearing as a one-waiter futex wake when the thread retires, which is what a joining thread waits on.[^c9][^c10] The exit signal in the low byte is the kernel's, and an exit signal other than `SIGCHLD` is recorded for the creator's whole process before the child exists only for a child without `CLONE_THREAD` or `CLONE_PARENT` and only on a backend whose host-timed signals are physical, because there it would otherwise arrive at a host-chosen moment inside a later gated wait.[^c11] A new task starts with no parent-death signal, as on Linux, and Detcore's own record of such signals likewise starts empty for it.[^c12][^linux] A pidfd the kernel writes for the creator is tracked as such only when the child shares neither memory nor descriptor table with its creator and is no `vfork` child, in a sequentialized run.[^c13][^c18]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] Creation is never faked from a recording: the child is really created in both modes and then follows its own thread-start path, where replay takes over its recorded stream, and under a replayed schedule its starting priority comes from that schedule rather than from creation-time choice.[^c7][^c8] In a chosen-identifier run, a child that instead received a host identifier, which differs from run to run, also marks the run's preemption record as host-identified, so a later replay of that record is refused.[^c31] A plain sequentialized run needs no recording for this call, because the same schedule admits the same children in the same order.[^c7]

## Known gaps

- G1 (by design): a pidfd for a child that shares memory or its descriptor table with its creator, for a `vfork` child, or for any child in a run without sequentialized threads, is left untracked, as is a pidfd whose tracking was attempted and failed, because its word cannot be read back, its descriptor number is already tracked, or tracking itself errors, and pidfd calls through it answer `EBADF` even though the child itself is unaffected; there is no fallback for that descriptor.[^c13][^c14][^c18][^c33]
- G2 (unimplemented): for a creator in a nested pid namespace, a child with no identifier recorded in that namespace, or whose recorded identifier cannot be translated, cannot be reported to its creator, so a fail-closed run is refused rather than returning a wrong identifier; a run that allows unsupported operations instead answers the creator `ENOSYS` while the already-registered child exists and runs, and there is no in-call fallback in either case.[^c15][^c28][^c30]
- G3 (by design): a backend that cannot create the child at all refuses a fail-closed run rather than returning an error Linux would never give a valid `clone`; a run that allows unsupported operations instead gives the guest the backend's refusal errno, so such a guest has no in-call fallback on that backend in either case.[^c16][^c28][^c29]
- G4 (by design): in a run that chooses guest identifiers, a call whose stack pointer has no rewritten form, a child in a pid namespace the guest made earlier or whose namespace cannot be read, a call a guest seccomp filter could make fare otherwise in its rewritten form (or whose filter count cannot be established), or a call made when no unused chosen identifier is left gets a host-chosen identifier as made rather than a chosen one, an identifier that differs from run to run, and a run requiring every identifier to be chosen ends instead before any task exists; a call that cannot be issued without the untraced flag ends any chosen-identifier run, and there is no in-call fallback to a chosen identifier for such a call.[^c20][^c21][^c25][^c26][^c27][^c31]
- G5 (unimplemented): an exit signal beyond the last signal number, without `CLONE_THREAD` or `CLONE_PARENT`, ends a run requiring every identifier to be chosen before any task exists, although Linux answers the same call `EINVAL` and, in a run that allows host identifiers, the guest receives that kernel `EINVAL`; there is no in-call fallback in the refused run.[^c24][^c20]

## Discussion

[Start a discussion about `clone`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clone&body=Discussion+for+the+%60clone%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclone.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clone%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `clone` (56) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1641`
- Dispatch: `detcore/src/lib.rs:8209`

### Tests

Flag-combination validation, covering only the combinations the kernel itself refuses from the flags alone: `detcore/src/syscalls/threads.rs` (`only_the_kernels_flag_rules_refuse`) — interface fidelity. The clear-tid wake as a join edge: `detcore/src/scheduler.rs` (`a_clear_child_tid_wake_is_a_join_edge`) — interface fidelity. Chosen identifiers, the rewrite, and its refusals: `detcore/src/syscalls/chosen_ids.rs` (`refused_and_inexpressible_calls`, `invalid_original_signals_are_not_normalized_into_valid_clones`, `a_stack_pointer_clone3_cannot_express_is_inexpressible`, `the_rewrite_drops_only_clone_untraced`, `a_pid_namespace_depth_counts_from_hermits`) — what Hermit does and interface fidelity. Guest programs under tests/c cover plain cloning, a pidfd child, an untraced clone, and a non-default exit signal, and the guest program tests/c/chosen_guest_ids.c covers chosen guest identifiers — what Hermit does and interface fidelity.

### Citations

[^linux]: Linux `clone(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/fork.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1641 sha256:eff53fa4d4df46ff
[^c2]: detcore/src/lib.rs:8209-8217 sha256:869aba0e4be284b3
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8209-L8217
[^c3]: detcore/src/syscalls/threads.rs:2460-2470 sha256:35d742bcc5f1e6df
[^c4]: detcore/src/syscalls/threads.rs:10442-10453 sha256:be369ffb98884c09; detcore/src/syscalls/threads.rs:10406-10416 sha256:1827611be72dc252
[^c5]: detcore/src/syscalls/chosen_ids.rs:647-671 sha256:8a7b480bd0d7f36b
[^c6]: detcore/src/syscalls/threads.rs:10178-10202 sha256:97e3c452d611365e
[^c7]: detcore/src/tool_global.rs:12203-12207 sha256:de87d70e069024ac; detcore/src/tool_global.rs:12220-12226 sha256:b173a95f7f3f3896
[^c8]: detcore/src/lib.rs:6619-6635 sha256:f21deb87dbe99073
[^c9]: detcore/src/tool_global.rs:12163-12171 sha256:512a94acfb2c0336
[^c10]: detcore/src/scheduler.rs:11788-11797 sha256:bb74122fed35d19a
[^c11]: detcore/src/syscalls/threads.rs:2570-2577 sha256:cbecd639f406e336; detcore/src/syscalls/threads.rs:1006-1017 sha256:177e3aca7f5c3580
[^c12]: detcore/src/scheduler.rs:3073-3078 sha256:4ab19dbcec31933d
[^c13]: detcore/src/syscalls/threads.rs:10654-10663 sha256:f42198d980e5d9c9; detcore/src/syscalls/threads.rs:10680-10686 sha256:d8436d27fa02f1c9
[^c14]: detcore/src/syscalls/threads.rs:5529-5538 sha256:62e1e3db7af13966
[^c15]: detcore/src/syscalls/threads.rs:2777-2800 sha256:27794f0170bb7d43
[^c16]: detcore/src/syscalls/threads.rs:2685-2696 sha256:33db94879c9412bb
[^c17]: detcore/src/syscalls/threads.rs:2601-2612 sha256:b2a564eb72ed0250
[^c18]: detcore/src/syscalls/threads.rs:2498-2505 sha256:601a467855472096
[^c19]: detcore/src/syscalls/chosen_ids.rs:620-629 sha256:5215652c2f07e57a
[^c20]: detcore/src/syscalls/chosen_ids.rs:716-739 sha256:eff0295014949ed9; detcore/src/syscalls/chosen_ids.rs:810-813 sha256:8e9e9c0abe5d9843; detcore/src/syscalls/chosen_ids.rs:901-917 sha256:76c23b03bd4deca1
[^c21]: detcore/src/syscalls/chosen_ids.rs:145-152 sha256:2ed0115d74493ddd; detcore/src/syscalls/chosen_ids.rs:288-322 sha256:8835c0b31c90cbfc
[^c22]: detcore/src/tool_global.rs:7506-7520 sha256:0c9817f20d17bc2c
[^c23]: detcore/src/syscalls/threads.rs:2455-2459 sha256:884406f2a339087a; detcore/src/syscalls/threads.rs:5572-5590 sha256:89390955cc169506
[^c24]: detcore/src/syscalls/chosen_ids.rs:154-158 sha256:efdc09a625f68b71; detcore/src/syscalls/chosen_ids.rs:258-267 sha256:ddd137278197effd; detcore/src/syscalls/chosen_ids.rs:703-709 sha256:293a3a3bec9f84bd
[^c25]: detcore/src/syscalls/chosen_ids.rs:736-740 sha256:20c8203dbbbe0577; detcore/src/syscalls/chosen_ids.rs:856-860 sha256:83d22a1699fcbcfd; detcore/src/syscalls/chosen_ids.rs:879-891 sha256:5a398e580f984674
[^c26]: detcore/src/syscalls/chosen_ids.rs:825-839 sha256:cc0d0f6d50a9aa85
[^c27]: detcore/src/syscalls/chosen_ids.rs:816-821 sha256:97c93ddeae998213; detcore/src/syscalls/chosen_ids.rs:1920-1938 sha256:93f777d339850c63
[^c28]: detcore/src/syscalls/helpers.rs:86-105 sha256:1294df3d700e8421; detcore/src/syscalls/helpers.rs:6337-6350 sha256:54d8d0885982d43b
[^c29]: detcore/src/syscalls/threads.rs:2700-2714 sha256:2549ed5a3ec39310
[^c30]: detcore/src/syscalls/threads.rs:2752-2753 sha256:24fa1d204b3bc2ed
[^c31]: detcore/src/syscalls/chosen_ids.rs:943-968 sha256:947726f7e1d78e68
[^c32]: detcore/src/tool_global.rs:12271-12287 sha256:28362aad92a74be4; detcore/src/tool_global.rs:12290-12296 sha256:6c0044def6212fc3
[^c33]: detcore/src/syscalls/threads.rs:5539-5569 sha256:580a1a6089895dbd
[^c34]: detcore/src/syscalls/threads.rs:2511-2517 sha256:268c84db06b57bd0; detcore/src/syscalls/helpers.rs:5328-5347 sha256:16fc7f0a8c0c0568
[^c35]: detcore/src/syscalls/threads.rs:5768-5780 sha256:61e365f7da3b957c; detcore/src/syscalls/threads.rs:5781-5805 sha256:0c6f6836cc95da88

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
- 2026-11-19 · rev 403e350e9e82 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
