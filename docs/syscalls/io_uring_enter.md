---
schema: syscall-pages/v3
syscall: io_uring_enter
nr_x86_64: 426
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_uring_enter&body=Discussion+for+the+%60io_uring_enter%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_uring_enter.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_uring_enter%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
  detcore_class: determinized
  dispatch: "typed"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
# prose: human/agent-authored, machine-checked (syscall-pages.rs check); v3 only
prose:
  status: checked
  reviewed_at: 2026-11-16
  based_on: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
---

## In short

Natively, io_uring completes work asynchronously in shared ring memory, outside syscall order. Hermit refuses it with ENOSYS, so nothing is ever submitted or waited on. Known gaps: 2.

## Determinism strategy

**What is nondeterministic natively.** `io_uring_enter` submits queued operations and can wait for completions: the kernel completes ring work asynchronously, so which completions are visible, in which order, and after how long a wait are not functions of the guest's syscall sequence alone.[^linux]

**What Hermit does.** Detcore refuses the whole `io_uring` family — `io_uring_setup`, `io_uring_enter`, and `io_uring_register` — with `ENOSYS` at dispatch, unconditionally: these numbers are in the deterministic-refusal set (not in the smaller strict-only subset), and the dispatch arm returns the error before any handler or guest injection can run.[^c1][^c2][^c3][^c4] The reason is structural: completions would land in shared ring memory outside the serialized syscall order Detcore schedules, and Detcore has no scheduler model for them.[^c4]

**Interface fidelity.** No submission is accepted and no completion is waited for: the refusal is by syscall number regardless of the fd or flags passed, so an inherited or received ring fd gains nothing.[^c4] The refusal is tested to happen before the syscall can be injected into the guest on backend paths that inject syscalls.[^c5][^c6]

**Record & replay.** All three numbers are subscribed in both run and record/replay modes (code facts in the Appendix).[^c4] The result is a constant (`ENOSYS`), so replay reproduces it exactly and there is no ring state to record.[^c4]

## Known gaps

- G1 (unimplemented): there is no deterministic ring model, so no submission or completion path exists to fall back to within io_uring.[^c4] Guests that require io_uring cannot run; guests with an `epoll`, POSIX AIO, or synchronous fallback never reach this call on that path. Supporting rings would mean Detcore owns completion scheduling — a design change, not a handler addition.
- G2 (by design): a guest that obtained a ring fd outside Hermit's control (passed across exec or received from another process) still gets `ENOSYS` here, where a native kernel would submit to the ring.[^c4]

## Discussion

[Start a discussion about `io_uring_enter`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_uring_enter&body=Discussion+for+the+%60io_uring_enter%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_uring_enter.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_uring_enter%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `io_uring_enter` (426) as determinized-refused via typed dispatch; deterministic refusal: true; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1746`
- Dispatch: `detcore/src/lib.rs:8090`

### Tests

Refusal precedes guest injection: `detcore/src/io_buffers.rs` (`io_uring_refusal_never_reaches_guest_injection`) — what Hermit does, and interface fidelity. Guest-level fallback is tracked by manifest custom cell `c-programs/io-uring-fallback` — known gaps.

### Citations

[^linux]: Linux `io_uring_enter(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/io_uring/io_uring.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1746 sha256:1c1981b8b744f325
[^c2]: detcore/src/syscall_classification.rs:1555-1564 sha256:d6fb9993d44cbe48
[^c3]: detcore/src/lib.rs:464-465 sha256:dfe85e842e0e8a89
[^c4]: detcore/src/lib.rs:8089-8092 sha256:5508f1a66cc8ff30
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8089-L8092
[^c5]: detcore/src/io_buffers.rs:1626-1631 sha256:bbd8dc0663f6afd6
[^c6]: detcore/src/io_buffers.rs:1817-1835 sha256:89e8c68d0a1a09e2

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-16 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-16 · rev 8e48c1c6605b · summary-first layout (In short, Known gaps, Appendix); claims unchanged.
