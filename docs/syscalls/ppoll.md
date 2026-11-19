---
schema: syscall-pages/v3
syscall: ppoll
nr_x86_64: 271
related:
  - https://github.com/rrnewton/hermit/issues/26
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ppoll&body=Discussion+for+the+%60ppoll%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fppoll.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ppoll%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, `ppoll` waits for descriptor readiness under a temporary signal mask, so its answer depends on host readiness timing, host clocks, and which signals are pending when the mask is in place. Hermit determinizes the wait with probes at scheduler turns against a logical deadline, applies a named temporary mask from a private sanitized copy, and on the wait it polls itself writes back the time left from logical time. Known gaps: 6.

## Determinism strategy

**What is nondeterministic natively.** Like its `poll` sibling, `ppoll` blocks until a named descriptor is ready, a timeout given as a timespec expires, or a signal arrives; it also replaces the thread's signal mask for the duration of the wait, so the count, the readiness bits, the time left written back, and whether a pending signal interrupts the wait all depend on host readiness, host time, and host signal timing.[^linux]

**What Hermit does.** Detcore determinizes `ppoll` by turning the wait into readiness probes with a defined place in the schedule: typed dispatch reaches a handler that first reads and validates the timespec timeout, then chooses between a zero-timeout probe, a wait the kernel owns, and an internal wait Hermit polls itself, by configuration; a masked call under in-guest signal delivery instead takes a gated path of its own, which reads the timeout there.[^c1][^c2][^c4][^c6] The internal wait observes logical time once to fix its deadline and retries the call as nonblocking probes at scheduler turns, filing the wait as internal polling that a signal may interrupt.[^c7][^c8] Where the call names a temporary mask, Hermit reads it at entry, notes it for its signal shadow, and probes with a private copy of it, sanitized so the guest mask cannot block Hermit's own preemption signal.[^c5][^c14] On a backend that runs the call itself and reports the guest's signal state, a masked wait is instead served by that backend's own probes, each carrying the mask so a pending signal it lets in can end the probe with `EINTR` and one it blocks never does.[^c16][^c17]

**Interface fidelity.** The timespec is validated as Linux validates it: a negative seconds or nanoseconds field, or a nanoseconds field naming a whole second or more, fails with `EINVAL`, and a mask whose size the kernel rejects likewise fails with `EINVAL` before any wait.[^c3][^c5][^linux] On a backend that reports the kernel's signal state, a signal that ends the internal wait returns the restart-without-a-handler form that a handler turns into `EINTR`, on the masked internal wait as on the unmasked one.[^c19] Where that reporting is absent and there is no mask, a scheduler wake ends the wait with `EINTR`, the interrupt errno the wait files, except on a backend that restarts a woken call only through the syscall-restart errno, where the wake returns that form when no handler runs; with a mask, each probe carries the private copy, so the kernel applies the temporary mask inside the probe and a pending signal it lets in can end that probe.[^c8][^c14][^c21] On that internal wait the time left is written back from logical time elapsed since the wait started, saturating at nothing left, and a failed write-back leaves the call's own result standing, with the interrupted case named under Known gaps, while a call that began with a zero timeout gets no write-back at all.[^c9] As with `poll`, what is ready is decided by running the guest's call as the probe, not by modelling each descriptor kind.[^c8]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] Under record and replay every wait that may block is a kernel-owned wait, masked or not, so its answer comes from the recording and replay never asks the kernel to install the temporary mask again; a recorded interruption is instead delivered under the mask read at entry.[^c6][^c12] A zero-timeout call under record and replay records or replays its probe like a plain call.[^c12]

## Known gaps

- G1 (unimplemented): a masked wait on a backend that cannot interrupt a blocked wait for a signal, and does not serve masked waits itself, is refused by name: a fail-closed run stops, and a run that allows unsupported operations answers `ENOSYS`, which this call never fails with natively; there is no fallback that keeps the temporary mask atomic, though the same wait without a mask is served.[^c11]
- G2 (by design): when the private mask and timeout copies have nowhere to go — no room below the stack's red zone and every such window covered by the call's own descriptor array or timeout — a fail-closed run refuses the call. A run that allows unsupported operations answers by path instead: a zero-timeout probe and a wait the kernel owns go on with the guest's own mask, which a sibling thread may still change during the wait, while the masked internal wait Hermit polls itself and the backend-served masked wait answer `ENOSYS`, which this call never fails with natively.[^c10][^c14][^c20]
- G3 (by design): between probes of a masked internal wait the kernel holds the thread's own mask, not the temporary one Linux installs for the whole call, and a signal the kernel posts from outside the guest's schedule, child exit included, can end a probe at a turn host timing chooses.[^c13]
- G4 (by design): a wait the kernel owns — every wait that may block under record and replay, and any wait without sequentialized threads — runs as the guest's raw call with a private copy only for the mask, so the kernel itself updates the guest timeout and the time left the guest reads back is the host clock's, not logical time; in replay that recorded remainder is served back rather than recomputed.[^c6][^c15]
- G5 (unimplemented): on a backend that runs the call itself, a pending signal the temporary mask lets in that the backend cannot act on inside the call — one that would stop the process, or an ignored one pending beside a signal that ends the call — is refused by name rather than answered as Linux answers it, and there is no fallback inside the call; only when every such signal is one the process ignores are they discarded, as Linux discards them, and the wait goes on instead of being refused.[^c18]
- G6 (unimplemented): with a finite timeout, when a signal ends the internal wait in its restart form and the time-left write then faults, Hermit keeps that restart form, so with no handler the call resumes from a remainder the guest never received; Linux instead reports `EINTR` there, since the call cannot then be restarted from an updated timeout.[^c9][^linux]

## Discussion

[Start a discussion about `ppoll`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ppoll&body=Discussion+for+the+%60ppoll%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fppoll.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ppoll%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `ppoll` (271) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1828`
- Dispatch: `detcore/src/lib.rs:8430`

### Tests

Timespec validation and units: `detcore/src/syscalls/io.rs` (`ppoll_timeout_uses_timespec_units`, `ppoll_timeout_rejects_invalid_timespecs`) — interface fidelity. Masked waits under the temporary mask, and their refusal when no probe can be staged: `detcore/src/tool_global/tests/gated_wait_turn_tests/temporary_mask_turn_tests.rs` (`under_the_gate_a_masked_ppoll_follows_linux_order_and_waits_under_its_temporary_mask`, `under_the_gate_a_masked_ppoll_on_a_tight_stack_is_refused_without_a_probe`) — what Hermit does and interface fidelity. Handler retries after a descriptor is replaced: `detcore/src/tool_global/tests/gated_wait_turn_tests/temporary_mask_turn_tests.rs` (`poll_retries_refresh_guest_held_authority_after_a_parked_fd_replacement`) — what Hermit does.

### Citations

[^linux]: Linux `ppoll(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/select.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1828 sha256:740b4e35218266a2
[^c2]: detcore/src/lib.rs:8430 sha256:0b0bce9538b9d1e7
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8430
[^c3]: detcore/src/syscalls/io.rs:847-854 sha256:ff820c79e1e62b6a
[^c4]: detcore/src/syscalls/io.rs:2919-2933 sha256:0979a4ae82cd22b5
[^c5]: detcore/src/syscalls/io.rs:912-931 sha256:5a58d957a7fe9548; detcore/src/syscalls/io.rs:1235-1238 sha256:3f3658f8ea97ecde
[^c6]: detcore/src/syscalls/io.rs:1251-1257 sha256:6b37b0f17f2cf730
[^c7]: detcore/src/syscalls/io.rs:895-906 sha256:7f7039d55995460f
[^c8]: detcore/src/syscalls/io.rs:3293-3312 sha256:274d2e6ef8f9ac03
[^c9]: detcore/src/syscalls/io.rs:3719-3737 sha256:4681f4ebb5b5945b; detcore/src/syscalls/io.rs:3012-3015 sha256:2b698ee8cc7ffa96
[^c10]: detcore/src/syscalls/io.rs:3166-3193 sha256:0b71490391b120db
[^c11]: detcore/src/syscalls/io.rs:3315-3328 sha256:ea37c8c14fa2fd9e; detcore/src/syscalls/io.rs:3361-3366 sha256:6a8f41228f27d699
[^c12]: detcore/src/syscalls/io.rs:2981-2987 sha256:0637b9ddbce26333; detcore/src/syscalls/io.rs:3103-3111 sha256:6ca08331a343c810
[^c13]: detcore/src/syscalls/io.rs:3512-3518 sha256:e047eb190f02abcd; detcore/src/syscalls/io.rs:3550-3551 sha256:034bebe77a07fd0b
[^c14]: detcore/src/syscalls/io.rs:3205-3233 sha256:3d475a2252804c1d
[^c15]: detcore/src/syscalls/io.rs:3016-3023 sha256:6f6a5b7c782cc495; detcore/src/syscalls/io.rs:3087-3111 sha256:394e30ffe6bb6a1b
[^c16]: detcore/src/syscalls/io.rs:3249-3257 sha256:e3797e23a015ac41
[^c17]: detcore/src/syscalls/io.rs:3619-3644 sha256:0568f82001bd4331
[^c18]: detcore/src/syscalls/io.rs:3330-3344 sha256:b32c8cfe58d51822; detcore/src/syscalls/io.rs:3410-3444 sha256:a0200b46be05f719
[^c19]: detcore/src/syscalls/helpers.rs:2405-2409 sha256:7bf1c07aa02a0505; detcore/src/syscalls/io.rs:3580-3591 sha256:76ff2ce0e0eea903; detcore/src/syscalls/helpers.rs:3325-3340 sha256:8262b911d6d198a9
[^c20]: detcore/src/syscalls/io.rs:3670-3677 sha256:4561bb96f545d84f; detcore/src/syscalls/io.rs:3432-3436 sha256:386ea9715e44eaa5
[^c21]: detcore/src/syscalls/helpers.rs:4638-4644 sha256:b736b2502a415df5; detcore/src/syscalls/helpers.rs:3777-3785 sha256:cfcd45667977ac30

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-17 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
