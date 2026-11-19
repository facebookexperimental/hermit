---
schema: syscall-pages/v3
syscall: epoll_pwait
nr_x86_64: 281
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_pwait&body=Discussion+for+the+%60epoll_pwait%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_pwait.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_pwait%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, this wait ends with whichever ready events, timeout, or signal host timing delivers first, under a signal mask swapped in only for the wait. Hermit polls with nonblocking probes to a logical-time deadline and decides signal interruption against the mask read at entry, so other guest threads can run while it waits. Known gaps: 4.

## Determinism strategy

**What is nondeterministic natively.** `epoll_pwait` is the sibling of `epoll_wait` that also installs a temporary signal mask for the duration of the wait: the kernel sets the given mask, sleeps until a listed descriptor is ready, the timeout expires, or a signal the temporary mask lets in arrives, and then restores the previous mask unless the wait ended for a signal.[^linux] Which outcome a run gets, and which events it copies out, therefore depend on host timing, peer activity, and which signals happen to be pending or sent during the wait.

**What Hermit does.** Detcore determinizes the call by routing it according to whether it names a mask and whether it can wait: with no mask it is semantically the `epoll_wait` wait and takes the same internal polling or shielded kernel-wait paths, converting a waiting timeout to a logical-time deadline and retrying with nonblocking probes under sequentialized threads.[^c1][^c2][^c3][^c4][^c9]

With a mask, the serviced forms never let the guest sleep through its schedule turn unexamined: under sequentialized threads the handler reads and sanitizes the mask at entry and polls with probes that carry no mask, deciding interruption against that entry mask, while in record and replay modes the kernel owns the wait outside the turn under a private copy of the mask that no sibling thread can rewrite.[^c5][^c6][^c7] Under in-guest signal delivery a masked call likewise polls with maskless probes and decides under its temporary mask, with a single maskless probe for a call that cannot wait.[^c3]

A masked call in any remaining configuration keeps its turn after an entry observation: a nonblocking call probes once there, and a waiting call runs in the host kernel inside the turn.[^c3]

**Interface fidelity.** The timeout keeps the millisecond meaning of `epoll_wait`: a zero timeout only probes and is never ended by a pending signal, a negative timeout sets no deadline, and an expired finite timeout returns a count of zero events.[^c4][^c5][^c9] On the polled and recorded masked paths the mask size is checked first and fails with `EINVAL`, then the mask is read and an unreadable mask fails with `EFAULT`, in each case before the epoll descriptor is examined.[^c5][^c7] A signal the temporary mask blocks never ends the polled wait and is delivered after it, while a signal the temporary mask lets in ends it with `EINTR`, with delivery under the temporary mask where the guest's own mask would keep it pending.[^c6] One exception is built into that sanitizing step on the polled masked path: the bit for the internal preemption signal is cleared from the entry mask before the wait decides interruption, so that signal stays unblocked for the wait even when the guest mask blocks it.[^c12] The descriptor, event-array, and maximum-events arguments are checked by the kernel probe or kernel wait Detcore runs, and ready events are written by that kernel call when it returns them.[^c4][^c6][^linux]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] An unmasked wait in those modes takes the shielded recorded kernel wait, and a masked wait sleeps outside the turn under its private mask copy while the scheduler records its sleeping mask as unknown; when a replayed masked wait ends interrupted, the handler delivers the signal under the temporary mask.[^c3][^c7][^c10] Replay answers this wait from the recording, and re-runs the real wait only for sets marked `replay_kernel_side_effect`; Detcore nevertheless files this family outside the polling calls it treats in replay as answered without blocking, so the scheduler still runs it as a call that may block.[^c7][^c11]

## Known gaps

- G1 (unimplemented): a masked call that can wait, in a configuration with no serviced polling or recorded form above, still waits in the host kernel inside its schedule turn, so no other guest thread can run to make an event ready and virtual timer expiry cannot be published during that wait; the handler refuses that host wait by name where virtual timer descriptors are involved, and otherwise the guest fallback is the unmasked sibling `epoll_wait` or a nonblocking call.[^c3]
- G2 (by design): ready events come from probes of the host kernel epoll instance, in the order that instance reports them, so readiness produced outside the guest on external descriptors is a host-timing input to the result rather than something Detcore synthesizes or reorders.[^c4]
- G3 (unimplemented): in record and replay modes a masked call whose private mask copy has nowhere to go — no stack scratch below the red zone and every red-zone window covered by the events buffer — a fail-closed run refuses the call, with no in-call fallback; when unsupported operations are allowed it instead forwards the guest mask itself for the kernel wait.[^c8]
- G4 (unimplemented): on the polled masked path, when a signal the temporary mask lets in ends the wait but there is no room to stage the private cells needed to deliver it under that mask — a stack pointer too close above unreadable memory with an unreadable red zone — the call returns the interrupt result without its handler having run, and the signal stays pending under the guest's own mask until the guest unblocks it; there is no in-call fallback.[^c6]

## Discussion

[Start a discussion about `epoll_pwait`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_pwait&body=Discussion+for+the+%60epoll_pwait%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_pwait.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_pwait%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `epoll_pwait` (281) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1662`
- Dispatch: `detcore/src/lib.rs:8438`

### Tests

Masked polling under the delivery gate, including ready, interrupted, and timed-out outcomes: `detcore/src/tool_global/tests/gated_wait_turn_tests/temporary_mask_turn_tests.rs` (`under_the_gate_a_masked_epoll_pwait_follows_ep_poll_with_d37_s_exception`) — what Hermit does, and interface fidelity. The interrupt and restart errnos: `detcore/src/syscalls/helpers.rs` (`signal_interruption_errno_matches_linux_restart_policy`, `kernel_restart_errno_matches_linux_restart_policy`) — interface fidelity.

### Citations

[^linux]: Linux `epoll_pwait(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/eventpoll.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1662 sha256:d4ed4463d3455e52
[^c2]: detcore/src/lib.rs:8438 sha256:664c8a6bf3b2ff72
[^c3]: detcore/src/syscalls/io.rs:4057-4103 sha256:04ee20abc4247de9
[^c4]: detcore/src/syscalls/io.rs:4277-4313 sha256:3939cc77b23bfeeb
[^c5]: detcore/src/syscalls/io.rs:4105-4147 sha256:3b245d8a70139fd0
[^c6]: detcore/src/syscalls/io.rs:4149-4196 sha256:fc1c5c641c0f326b
[^c7]: detcore/src/syscalls/io.rs:4198-4249 sha256:8761fb5dbc3a8401
[^c8]: detcore/src/syscalls/io.rs:4251-4275 sha256:639bb2c503409c4a; detcore/src/syscalls/helpers.rs:6338-6365 sha256:eafbc4d65f9a0e77
[^c9]: detcore/src/syscalls/helpers.rs:2440-2468 sha256:a374ae7e78fc5501
[^c10]: detcore/src/syscalls/io.rs:4415-4430 sha256:a4c01c4c712baa99
[^c11]: detcore/src/syscalls/helpers.rs:2042-2054 sha256:0f84c7a03dc9ccc4; detcore/src/syscalls/helpers.rs:400-412 sha256:12c8e4c0914d7258
[^c12]: detcore/src/syscalls/io.rs:1235-1238 sha256:3f3658f8ea97ecde

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-17 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
