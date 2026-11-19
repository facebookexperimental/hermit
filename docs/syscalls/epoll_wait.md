---
schema: syscall-pages/v3
syscall: epoll_wait
nr_x86_64: 232
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_wait&body=Discussion+for+the+%60epoll_wait%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_wait.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_wait%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, which descriptors are reported ready and when the wait ends depend on host timing, concurrent activity, and signal arrival. Hermit turns the wait into repeated nonblocking probes against a deadline in logical time, yielding the schedule between probes so guest threads can make descriptors ready. Known gaps: 2.

## Determinism strategy

**What is nondeterministic natively.** `epoll_wait` sleeps in the kernel until a descriptor on the interest list becomes ready, the timeout expires, or a signal arrives, and then copies out the events that were ready at wakeup.[^linux] The count and contents of the returned array, and whether the call returns at all, therefore depend on host timing, on activity by threads and peers outside the call, and on real elapsed time.

**What Hermit does.** Detcore determinizes the wait in sequentialized runs by never sleeping in the kernel while holding the schedule turn: a waiting call converts its millisecond timeout into a deadline in logical time and retries as a nonblocking probe under an internal polling resource, yielding the turn between probes so other guest threads can run and make a descriptor ready.[^c1][^c2][^c4][^c7] A call that cannot block takes a single probe at once, after any expired virtual timer descriptors have been made readable.[^c4]

Outside sequentialized runs, and in record and replay modes, the wait instead runs as a kernel wait through the shielded record-or-replay path, with signals the guest ignores held back for the duration, matching the native rule that ignored signals never end this wait.[^c3][^c6][^linux] In record and replay modes even a nonblocking call still yields a scheduler turn first, so a polling loop cannot monopolize the guest between preemptions.[^c3]

**Interface fidelity.** The timeout is a millisecond count: a zero timeout never waits, a negative timeout sets no deadline, and a finite timeout that expires with nothing ready returns a count of zero events.[^c4][^c5] Ready events are written to the guest array by the kernel probe that found them, and the descriptor, event-array, and maximum-events arguments are checked by that kernel call rather than by Detcore.[^c4][^linux] A signal that ends the wait ends it with `EINTR` and the call is not restarted afterward; the nonblocking probe form and the timeout answer encode exactly that interrupt result and a zero-event timeout return.[^c5]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] In those modes the waiting call runs as a recorded kernel wait outside the runnable pool through the shielded path.[^c3][^c6] Replay answers this wait from the recording, and re-runs the real wait only for sets marked `replay_kernel_side_effect`. Detcore nevertheless files it outside the polling calls it treats in replay as answered without blocking, so the scheduler still runs it as a background call that may block while that answer is served.[^c8]

## Known gaps

- G1 (by design): ready events come from probes of the host kernel epoll instance, in the order that instance reports them, so readiness produced outside the guest on external descriptors is a host-timing input to the result rather than something Detcore synthesizes or reorders.[^c4]
- G2 (by design): a finite timeout expires in logical time, at a deadline observed from the guest clock, so its wall-clock duration under Hermit need not match a native wait with the same timeout.[^c7]

## Discussion

[Start a discussion about `epoll_wait`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_wait&body=Discussion+for+the+%60epoll_wait%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_wait.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_wait%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `epoll_wait` (232) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1665`
- Dispatch: `detcore/src/lib.rs:8440`

### Tests

The interrupt result and the no-restart rule: `detcore/src/syscalls/helpers.rs` (`signal_interruption_errno_matches_linux_restart_policy`, `kernel_restart_errno_matches_linux_restart_policy`) — interface fidelity. Ready-event ordering across several descriptors, edge-triggered delivery, and one-shot delivery and rearming are also exercised end to end by the guest program tests/c/epoll_determinism.c, run repeatedly for identical output by the integration test hermit-cli/tests/epoll_determinism.rs — what Hermit does, and interface fidelity.[^c4]

### Citations

[^linux]: Linux `epoll_wait(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/eventpoll.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1665 sha256:bfdbf2a22e3858a9
[^c2]: detcore/src/lib.rs:8439-8440 sha256:7c1671ef1a801fb4
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8439-L8440
[^c3]: detcore/src/syscalls/io.rs:4637-4655 sha256:008e67c6c4db64a6
[^c4]: detcore/src/syscalls/io.rs:4657-4675 sha256:5709fa9d89869825
[^c5]: detcore/src/syscalls/helpers.rs:2418-2438 sha256:4cd11cf7083f25bd
[^c6]: detcore/src/syscalls/io.rs:4415-4430 sha256:a4c01c4c712baa99
[^c7]: detcore/src/syscalls/helpers.rs:8175-8183 sha256:53ea3fafc31f865d
[^c8]: detcore/src/syscalls/helpers.rs:2042-2054 sha256:0f84c7a03dc9ccc4; detcore/src/syscalls/helpers.rs:400-412 sha256:12c8e4c0914d7258

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-17 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
