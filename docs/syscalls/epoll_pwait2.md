---
schema: syscall-pages/v3
syscall: epoll_pwait2
nr_x86_64: 441
related:
  - https://github.com/rrnewton/hermit/issues/345
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_pwait2&body=Discussion+for+the+%60epoll_pwait2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_pwait2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_pwait2%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 403e350e9e82ee540529f097daccae6a60832d07
  detcore_class: determinized
  dispatch: "sysno-guard"
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

Natively, this wait ends with whichever ready events, timeout, or signal host timing delivers first, under an optional temporary signal mask. Hermit reaches it by syscall-number guard and then polls it like its millisecond sibling, with the finer timeout converted to a logical-time deadline. Known gaps: 4.

## Determinism strategy

**What is nondeterministic natively.** `epoll_pwait2` waits exactly as `epoll_pwait` does — ready descriptors, timeout expiry, or a signal let in by an optional temporary mask decides the outcome — except that its timeout is a timespec with finer granularity than the millisecond count of its siblings.[^linux] The events copied out and the outcome itself therefore depend on host timing, peer activity, and signal arrival during the wait, as they do for `epoll_wait`.

**What Hermit does.** Detcore determinizes the call even though it has no typed form in the pinned instrumentation revision: classification marks it determinized, a syscall-number guard ahead of the typed dispatch routes it to its handler, and the handler then treats it as its sibling `epoll_pwait` apart from reading the timeout from a timespec.[^c1][^c2][^c3]

A waiting call under sequentialized threads polls with zero-timeout probes of the sibling `epoll_pwait` carrying no mask, against a deadline in logical time from that timespec, sharing the masked-wait decision of `epoll_pwait` when a mask is named; in record and replay modes a waiting call instead waits outside the turn, masked under a private copy of its mask and unmasked through the shielded recorded wait.[^c4][^c5][^c6] Under in-guest signal delivery a masked call likewise polls with maskless probes and decides under its temporary mask, with a single maskless probe for a call that cannot wait.[^c3][^c12]

A call in any remaining configuration, and any call that cannot wait, keeps its turn after a scheduler yield: a nonblocking call probes once there and a waiting call runs in the host kernel inside the turn, shielded from ignored signals when it waits under the thread's own mask.[^c4]

**Interface fidelity.** There is no flags argument to honor: the call takes a descriptor, an event array, a maximum-events count, a timespec timeout pointer, and an optional mask with its size.[^c3][^linux] A null timeout pointer waits without a deadline and a zero timespec cannot wait, while an unreadable timespec fails with `EFAULT` and an out-of-range timespec fails with `EINVAL`, checked before the mask size, the mask read, and the epoll descriptor on the polled and recorded paths.[^c5][^c6][^c8] Mask handling, signal interruption, event writeback, and the zero-event timeout return are then those of `epoll_pwait` on the polled path, because the probes and the masked decision there are literally that sibling call and its shared wait.[^c5] On the recorded masked path the kernel wait is instead the raw `epoll_pwait2` call itself, issued by syscall number under the staged private mask copy.[^c6] The same sanitizing exception as the sibling applies on the polled masked path: the bit for the internal preemption signal is cleared from the entry mask, so that signal stays unblocked for the wait even when the guest mask blocks it.[^c10]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix), reached there by the same number guard rather than by a typed arm.[^c2] Recorded masked waits sleep outside the turn under a private mask copy staged clear of the event array and the timeout, and when a replayed masked wait ends interrupted the handler delivers the signal under the temporary mask.[^c6] Replay answers this wait from the recording, and re-runs the real wait only for sets marked `replay_kernel_side_effect`; Detcore nevertheless files this family outside the polling calls it treats in replay as answered without blocking, so the scheduler still runs it as a call that may block.[^c6][^c9]

## Known gaps

- G1 (unimplemented): a waiting call in a configuration with no serviced polling or recorded form above still waits in the host kernel inside its schedule turn, so no other guest thread can run to make an event ready and virtual timer expiry cannot be published during that wait; the handler refuses that host wait by name where virtual timer descriptors are involved, and otherwise the guest fallback is a sibling wait such as `epoll_wait` or a nonblocking call.[^c4]
- G2 (by design): ready events come from probes of the host kernel epoll instance, in the order that instance reports them, so readiness produced outside the guest on external descriptors is a host-timing input to the result rather than something Detcore synthesizes or reorders.[^c5]
- G3 (unimplemented): in record and replay modes a masked call whose private mask copy has nowhere to go — no stack scratch below the red zone and the red zone filled by the events buffer and the timeout — a fail-closed run refuses the call, with no in-call fallback; when unsupported operations are allowed it instead forwards the guest mask itself for the kernel wait.[^c7]
- G4 (unimplemented): on the polled masked path, when a signal the temporary mask lets in ends the wait but there is no room to stage the private cells needed to deliver it under that mask — a stack pointer too close above unreadable memory with an unreadable red zone — the call returns the interrupt result without its handler having run, and the signal stays pending under the guest's own mask until the guest unblocks it; there is no in-call fallback.[^c11]

## Discussion

[Start a discussion about `epoll_pwait2`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_pwait2&body=Discussion+for+the+%60epoll_pwait2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_pwait2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_pwait2%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `epoll_pwait2` (441) as determinized via sysno-guard dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1663`
- Dispatch: `detcore/src/lib.rs:8009`

### Tests

Masked polling under the delivery gate through this handler: `detcore/src/tool_global/tests/gated_wait_turn_tests/temporary_mask_turn_tests.rs` (`under_the_gate_a_masked_epoll_pwait2_takes_the_same_path`) — what Hermit does, and interface fidelity. Timespec conversion and rejection of out-of-range timeouts: `detcore/src/syscalls/io.rs` (`ppoll_timeout_uses_timespec_units`, `ppoll_timeout_rejects_invalid_timespecs`) — interface fidelity.

### Citations

[^linux]: Linux `epoll_pwait2(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/eventpoll.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1663 sha256:e80669ef0e9bb465
[^c2]: detcore/src/lib.rs:8003-8011 sha256:ef8b797448b05a28
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8003-L8011
[^c3]: detcore/src/syscalls/io.rs:4315-4346 sha256:198a6a56d5b1cb96
[^c4]: detcore/src/syscalls/io.rs:4347-4413 sha256:835f52b67f84ec64
[^c5]: detcore/src/syscalls/io.rs:4452-4511 sha256:1b4b28cce42224c2
[^c6]: detcore/src/syscalls/io.rs:4550-4609 sha256:aee5fa6078df7490
[^c7]: detcore/src/syscalls/io.rs:4611-4635 sha256:ed22bd50ebfcb4e8; detcore/src/syscalls/helpers.rs:6338-6365 sha256:eafbc4d65f9a0e77
[^c8]: detcore/src/syscalls/io.rs:847-854 sha256:ff820c79e1e62b6a
[^c9]: detcore/src/syscalls/helpers.rs:2042-2054 sha256:0f84c7a03dc9ccc4; detcore/src/syscalls/helpers.rs:400-412 sha256:12c8e4c0914d7258
[^c10]: detcore/src/syscalls/io.rs:1235-1238 sha256:3f3658f8ea97ecde
[^c11]: detcore/src/syscalls/io.rs:4149-4196 sha256:fc1c5c641c0f326b
[^c12]: detcore/src/syscalls/io/temporary_mask_forms.rs:1490-1528 sha256:1e060c6c2facb54a

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-17 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
