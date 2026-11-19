---
schema: syscall-pages/v3
syscall: poll
nr_x86_64: 7
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+poll&body=Discussion+for+the+%60poll%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpoll.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60poll%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, `poll` waits for descriptor readiness that arrives on host time, and a signal or the timeout can end the wait at a moment host scheduling chooses. Hermit answers the wait from readiness probes taken at scheduler turns against a logical deadline, so a repeated schedule repeats the answer. Known gaps: 2.

## Determinism strategy

**What is nondeterministic natively.** `poll` reports which of the named descriptors are ready and blocks until one is, the timeout expires, or a signal arrives, so the returned count, the readiness bits written back, and whether the call blocks at all depend on when producers, the host clock, and signal delivery happen to run.[^linux]

**What Hermit does.** Detcore determinizes `poll` by giving the wait a place in its deterministic schedule instead of letting one host wait decide it: typed dispatch reaches a handler with separate paths for a zero timeout, a wait the kernel owns, and a wait Hermit polls itself.[^c1][^c2][^c3][^c4] On the polled path the millisecond timeout becomes an absolute logical deadline and the call retries as nonblocking probes at scheduler turns until a probe reports readiness, an error, a signal, or that deadline.[^c6][^c7] A zero timeout cannot block, but it still takes a scheduler turn before its single probe. That turn rotates the caller within its priority band. It does not guarantee that a higher-priority busy poller lets its producer run.[^c3][^c9] Only a wait the kernel owns — under record and replay, or without sequentialized threads — blocks in the host kernel, and a plain sequentialized run never takes that path.[^c5]

**Interface fidelity.** The timeout is honored in Linux's shapes: zero returns after one probe, a negative timeout has no deadline, and a positive timeout ends the polled wait at its logical deadline.[^c6][^c7] Readiness itself — which descriptors are ready and with what bits, including entries the kernel ignores or reports in error — is decided by running the guest's own call as the probe, not by a Detcore model of each descriptor kind.[^c3][^c6][^linux] On a backend that reports the kernel's signal state, a signal that ends the polled wait returns the restart form that a handler turns into `EINTR` and that restarts the call when no handler runs; with a positive timeout that restart keeps the first deadline through the restart record the wait files.[^c12][^c6] Where that reporting is absent, a scheduler wake ends the wait with `EINTR` through the wait's filed interrupt errno, except on a backend that restarts a woken call only through the syscall-restart errno, where the wake returns that form when no handler runs (the rerun keeping its first deadline).[^c6][^c13] A wait the kernel owns that a signal ends is restarted or reported following the restart record it kept, keeping the first end time where Linux keeps it.[^c6][^c8][^linux]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] Under record and replay a wait that may block runs as a kernel wait whose answer is recorded, because replay cannot rely on the descriptors still existing, while a zero-timeout call is recorded or replayed as a plain call after its turn.[^c3][^c4][^c10] A sequentialized run needs no recording for this call: the same schedule reaches the same probes against the same logical deadline.[^c6][^c7]

## Known gaps

- G1 (by design): a positive-timeout wait the kernel owns keeps an end time read from the host clock when the call is first issued, outside record and replay, so a restart after a signal ends at that host end time rather than a logical one; the sequentialized polled path never reads it.[^c4][^c5]
- G2 (by design): readiness that only something outside the guest can produce is not made deterministic: Hermit fixes the turns at which the guest probes and observes it, but not when the outside producer acts, so which probe first sees an externally readied descriptor can still follow the outside arrival.[^c6]

## Discussion

[Start a discussion about `poll`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+poll&body=Discussion+for+the+%60poll%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpoll.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60poll%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `poll` (7) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1827`
- Dispatch: `detcore/src/lib.rs:8421`

### Tests

Kernel-owned versus polled waits and their restarts: `detcore/src/syscalls/io.rs` (`only_record_replay_and_unsequentialized_runs_poll_in_the_kernel`, `a_restarted_kernel_poll_waits_only_until_its_deadline`, `a_kernel_poll_without_a_deadline_restarts_by_running_again`, `a_kernel_poll_that_keeps_a_record_restarts_through_restart_syscall`) — what Hermit does, interface fidelity, and record & replay. The turn a zero timeout still takes: `detcore/src/syscalls/io.rs` (`zero_timeout_socket_poll_requests_a_strong_one_turn_yield`, `zero_timeout_non_socket_poll_keeps_the_existing_empty_turn`) — what Hermit does. Handler retries after a descriptor is replaced: `detcore/src/tool_global/tests/gated_wait_turn_tests/temporary_mask_turn_tests.rs` (`poll_retries_refresh_guest_held_authority_after_a_parked_fd_replacement`) — what Hermit does.

### Citations

[^linux]: Linux `poll(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/select.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1827 sha256:caa5ce19ddb8f1f2
[^c2]: detcore/src/lib.rs:8420-8421 sha256:522d15d3ca7ab32c
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8420-L8421
[^c3]: detcore/src/syscalls/io.rs:1466-1484 sha256:31a2a8ad7e2ce7f8
[^c4]: detcore/src/syscalls/io.rs:1485-1505 sha256:64eb5453fb353bbc
[^c5]: detcore/src/syscalls/io.rs:1407-1427 sha256:b5a77f343dad1696
[^c6]: detcore/src/syscalls/io.rs:3779-3793 sha256:37b370a19b2a6ca5
[^c7]: detcore/src/syscalls/helpers.rs:8175-8196 sha256:c6137d6fbb2ca81b
[^c8]: detcore/src/syscalls/io.rs:1323-1335 sha256:67b2fc9f7bde2848
[^c9]: detcore/src/syscalls/io.rs:113-130 sha256:793c3cb231ca70d5
[^c10]: detcore/src/syscalls/io.rs:3863-3880 sha256:ab139cf5afdd9eb3
[^c12]: detcore/src/syscalls/helpers.rs:2359-2379 sha256:d0e42374f196cec2; detcore/src/syscalls/helpers.rs:3325-3340 sha256:8262b911d6d198a9
[^c13]: detcore/src/syscalls/helpers.rs:4638-4644 sha256:b736b2502a415df5; detcore/src/syscalls/io.rs:3796-3837 sha256:5f411e4e12ad0b62

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-17 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
