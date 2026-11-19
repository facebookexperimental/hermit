---
schema: syscall-pages/v3
syscall: pselect6
nr_x86_64: 270
related:
  - https://github.com/rrnewton/hermit/issues/384
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pselect6&body=Discussion+for+the+%60pselect6%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpselect6.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pselect6%60%2C+or+in+what+the+page+says%3F%0A
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
  reviewed_at: 2026-11-17
  based_on: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
---

## In short

Natively, this wait's outcome is settled by host readiness, host elapsed time, and signal races under a signal mask swapped in just for the call. Hermit determinizes it by snapshotting that temporary mask at entry, polling scratch descriptor sets at scheduler turns against a virtual-time deadline, and writing back a virtual remaining time. Known gaps: 7.

## Determinism strategy

**What is nondeterministic natively.** Linux atomically installs a caller-supplied temporary signal mask, then waits until a descriptor is ready, the timeout expires by the host clock, or a signal the temporary mask lets in interrupts, and puts the caller's own mask back as the call returns.[^linux] Host scheduling and device timing therefore decide the returned count, the ready subset written back, whether and how long the call blocks, and which signal, if any, interrupts under the swapped mask, while the time not slept is written back into the timeout.[^linux]

**What Hermit does.** Detcore determinizes the call: typed dispatch reaches a handler that, in a sequentialized run, snapshots the temporary mask at entry and emulates the wait as repeated scheduler-polled probes over scratch copies of the descriptor sets, instead of one host-timed sleep under a mask read whenever the host gets to it.[^c1][^c2][^c6][^c8] The snapshot is what every interruption decision uses, so a sibling rewriting the caller's mask buffer afterwards cannot change what this call waits under.[^c6][^c8][^c18] A finite timeout becomes a logical-time deadline, and the remaining time written back comes from that virtual clock.[^c8][^c10]

Some forms stay with the kernel instead of the probes: a zero timeout is a single kernel call, a negative descriptor count is passed through for the kernel's own answer, and — with the further cases named under Known gaps — a count beyond one machine word sleeps in the kernel under a private copy of the entry mask, so no probe over-reads the guest bitmap and no sibling can swap the sleeping mask.[^c4][^c15] Without sequentialized threads the kernel likewise owns a masked wait, and an unmasked one runs behind a signal shield.[^c4]

**Interface fidelity.** In the main sequentialized path, validation follows Linux's order and strictness: the outer mask wrapper is copied and checked before the timeout is judged, a `timespec` with a negative field or a nanoseconds field of a whole second or more fails with `EINVAL` — unlike the `timeval` of `select`, nothing folds — and only then is the inner mask read.[^c4][^c3][^linux] The record and replay path orders its entry work the other way: it reads the entry mask first, before its delivery-room refusal check, having only peeked at the timeout to tell a zero timeout from a wait.[^c16]

An absent wrapper, or one whose inner pointer is null, names no mask and leaves the thread's own mask in place without its size being judged, while a real mask with a size other than the kernel's fails with `EINVAL` and a mask address that cannot be read fails with `EFAULT`.[^c5][^c6] The honored mask is the entry snapshot itself, with one deliberate exception: Hermit's own preemption signal is kept unblocked under it, so that signal alone can still arrive even if the guest's mask would block it.[^c6][^c7]

The descriptor sets are copied in and out through scratch as on the `select` page: the caller's sets change only when a probe succeeds or the deadline expires, a set that cannot be read fails with `EFAULT`, and a failed remaining-time write keeps the settled result, with the interrupted case named under Known gaps.[^c8][^c9][^c10] A turn probes before its pending-signal check, and a signal that only the temporary mask unblocks is delivered by the kernel under that mask at the end of the call rather than being left pending behind the caller's own mask, so its handler runs as it would natively; the private cells that delivery stages reuse scratch this call's own probes have already committed, so on every run that reaches the delivery they have somewhere to sit.[^c9] Where the backend reports no kernel signal state and no probe carries the mask, a scheduler wake can still end the call before its probe, returning `EINTR` itself.[^c9]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] A masked call that may wait sleeps under a private copy of the mask it read at entry, staged clear of the sets and timeout the kernel touches, so recording and replay wait under that entry mask rather than whatever the buffer holds when the kernel copies it.[^c11] Replay serves the recorded outcome without the kernel ever installing that mask, so an interruption by a signal only the temporary mask unblocks is finished with an injected zero-timeout `ppoll` under a scratch copy of the mask, delivering the signal as the recorded call did.[^c12]

## Known gaps

- G1 (by design): a descriptor count beyond one machine word does not take the scheduler-probed path at all — the wait stays with the kernel under a private copy of the entry mask, to avoid over-reading a bitmap whose copied length the kernel clamps to its own descriptor table.[^c15]
- G2 (unimplemented): when a private copy of the temporary mask has nowhere to sit clear of the call's own buffers, a fail-closed run refuses the masked call instead of serving it, and there is no fallback inside the call; a run that allows unsupported calls continues with the caller's buffer and an unknown sleeping mask instead.[^c13]
- G3 (unimplemented): a backend that runs the call itself can decline a masked call whose sets it does not serve, or a call whose mask lets in a pending signal it cannot act on inside the call; either way the guest meets a named refusal rather than the answer Linux gives, with no fallback inside the call.[^c14]
- G4 (unimplemented): a descriptor closed while another thread waits on it is not reported ready as on Linux — each probe is a fresh call, so the wait instead fails, as on the `select` page.[^c9]
- G5 (unimplemented): under record and replay, a masked call whose stack sits too close above unwritable memory for replay to deliver a signal that interrupts it is refused by name in a fail-closed run, recording and replay alike, rather than served; the fallback in a run that allows unsupported calls is to record the call as before, and a replay of an interrupted one then stops at the next syscall.[^c17][^c16]
- G6 (unimplemented): where the backend reports kernel signal state, a pending caught `SIGCHLD` that the temporary mask lets in can end this wait at a turn host timing chooses, because this wait holds no signal back until it returns; holding it as other waits do is not done here.[^c18]
- G7 (unimplemented): with a finite timeout, when a signal ends the emulated wait in its restart form and the remaining-time write then faults, Hermit keeps that restart form, so with no handler the call resumes from a remainder the guest never received; Linux instead reports `EINTR` there, since the call cannot then be restarted from an updated timeout.[^c10][^linux]

## Discussion

[Start a discussion about `pselect6`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pselect6&body=Discussion+for+the+%60pselect6%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpselect6.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pselect6%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `pselect6` (270) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1840`
- Dispatch: `detcore/src/lib.rs:8424`

### Tests

That this call stays in the determinized class: `detcore/src/syscall_classification.rs` (`representative_policies_stay_in_their_reviewed_sections`) — what Hermit does. Descriptor-set lengths following the raw interface: `detcore/src/syscalls/io.rs` (`pselect6_fd_set_lengths_follow_the_raw_linux_abi`) — interface fidelity. Which masked refusals name a signal and which name the descriptor sets: `detcore/src/syscalls/io.rs` (`a_masked_pselect6_refusal_names_a_signal_only_when_one_is_pending`, `a_masked_pselect6_of_more_than_64_descriptors_is_refused_for_its_sets`) — known gaps. That each select form ends only for a signal its mask lets in, under the in-guest delivery gate: `detcore/src/tool_global/tests/gated_wait_turn_tests/temporary_mask_turn_tests.rs` (`under_the_gate_each_select_form_ends_only_for_a_signal_its_mask_lets_in`) — interface fidelity.

### Citations

[^linux]: Linux `pselect6(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/select.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1840 sha256:d6ebbe60c7510d38
[^c2]: detcore/src/lib.rs:8422-8424 sha256:98c1dc96a01633bd
[^c3]: detcore/src/syscalls/io.rs:847-854 sha256:ff820c79e1e62b6a
[^c4]: detcore/src/syscalls/io.rs:1736-1752 sha256:62431e3a33334a70; detcore/src/syscalls/io.rs:1760-1791 sha256:8fc9c2ed71e294be; detcore/src/syscalls/io.rs:1828-1848 sha256:fddc062ddde76412
[^c5]: detcore/src/syscalls/io.rs:414-462 sha256:4a4c5991b6ed6b42
[^c6]: detcore/src/syscalls/io.rs:1950-1971 sha256:f5e3af55bf71a1d8
[^c7]: detcore/src/syscalls/io.rs:1235-1238 sha256:3f3658f8ea97ecde
[^c8]: detcore/src/syscalls/io.rs:1974-1981 sha256:5068129fcf7a8a2c; detcore/src/syscalls/io.rs:1996-2005 sha256:8288657c6920ff75; detcore/src/syscalls/io.rs:2052-2080 sha256:623e0380bb9bf7d9; detcore/src/syscalls/io.rs:2126-2142 sha256:2b7054343f534b63
[^c9]: detcore/src/syscalls/io.rs:2233-2255 sha256:986b88c02eff4f80; detcore/src/syscalls/io.rs:2426-2435 sha256:1c73cdf5fa213d57; detcore/src/syscalls/io.rs:2474-2517 sha256:69affac2f9ca8e4b
[^c10]: detcore/src/syscalls/io.rs:779-799 sha256:0cbe3253f3da7ded; detcore/src/syscalls/io.rs:2520-2554 sha256:9e1efceb881405fb
[^c11]: detcore/src/syscalls/io.rs:1591-1627 sha256:15731092de99e918
[^c12]: detcore/src/syscalls/io.rs:520-548 sha256:30d048ee8c6f4d92; detcore/src/syscalls/io.rs:1717-1734 sha256:5fd0bd92c7cc302d
[^c13]: detcore/src/syscalls/io.rs:1528-1562 sha256:3e13a53969bc6fbf
[^c14]: detcore/src/syscalls/io.rs:736-777 sha256:4d5dffe8fa6c99eb
[^c15]: detcore/src/syscalls/io.rs:1850-1854 sha256:eee72a26a924d2b2; detcore/src/syscalls/io.rs:1914-1926 sha256:0b135ae278e25410
[^c16]: detcore/src/syscalls/io.rs:1638-1644 sha256:35d29e0b609ca07f; detcore/src/syscalls/io.rs:1676-1685 sha256:214dbfdfb5c93b1e
[^c17]: detcore/src/syscalls/io.rs:1564-1589 sha256:907f60b51420f16f
[^c18]: detcore/src/syscalls/io.rs:2081-2097 sha256:498ecd7e2f6f793e

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-17 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
