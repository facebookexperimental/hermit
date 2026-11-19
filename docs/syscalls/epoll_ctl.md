---
schema: syscall-pages/v3
syscall: epoll_ctl
nr_x86_64: 233
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_ctl&body=Discussion+for+the+%60epoll_ctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_ctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_ctl%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, adding, changing, or removing an interest succeeds or fails according to the current interest list and descriptor state. Hermit serializes each change and forwards it to the host epoll instance, so a replayed run rebuilds the same interest list from the recorded changes. Known gaps: 2.

## Determinism strategy

**What is nondeterministic natively.** `epoll_ctl` mutates the interest list of an epoll instance: whether an add, modify, or delete operation succeeds depends on the list as it stands when the call runs, on the target descriptor still naming the same open file, and on limits and loop checks the kernel applies to the resulting list.[^linux] Concurrent mutation or descriptor reuse around the call can therefore change its result without any change to its arguments.

**What Hermit does.** Detcore determinizes the mutation by serializing it and forwarding it unchanged: typed dispatch reaches a handler that files an empty resource request and runs the operation, with its event payload, through the record-or-replay path against the host epoll instance.[^c1][^c2][^c3] Detcore keeps no interest-list model of its own for this call, so under strict execution the result depends only on the operation, the event payload, and the epoll and descriptor state the deterministic run has reconstructed up to that serialized point.[^c3]

**Interface fidelity.** The add, modify, and delete operations are honored by the kernel call Detcore forwards, which also performs the event-argument copy for the operations that take one.[^c3][^linux] Detcore validates neither the operation nor the descriptors nor the event payload itself, so every success and every refusal a guest observes from this call is the host kernel answer to the forwarded operation, returned unchanged.[^c3]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] Each forwarded answer is recorded, and replay reapplies only the recorded successes, so the epoll sets a replay holds are the sets the recording held rather than a fresh negotiation with the host kernel.[^c3]

## Known gaps

- G1 (by design): the interest list lives in the host kernel epoll instance, not in a Detcore model, so kernel-specific limits and nesting-loop decisions on forwarded operations are inherited from the host kernel rather than reproduced from guest state alone.[^c3]
- G2 (unmeasured): no dedicated unit test exercises this handler at this revision, and the integration guest credited in Tests exercises successful add, modify, and delete operations but not refused operations, so the forwarded-refusal claims rest on the handler code alone pending such a test.[^c3]

## Discussion

[Start a discussion about `epoll_ctl`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_ctl&body=Discussion+for+the+%60epoll_ctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_ctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_ctl%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `epoll_ctl` (233) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1659`
- Dispatch: `detcore/src/lib.rs:8437`

### Tests

Successful add, modify, and delete operations, including rearming and changed event tags, are exercised end to end by the guest program tests/c/epoll_determinism.c, run repeatedly for identical output by the integration test hermit-cli/tests/epoll_determinism.rs — what Hermit does, and interface fidelity. No dedicated unit test for this handler exists at this revision; that narrower absence is part of G2.[^c3]

### Citations

[^linux]: Linux `epoll_ctl(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/eventpoll.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1659 sha256:7533628e1d6d04aa
[^c2]: detcore/src/lib.rs:8437 sha256:4582d17ffc4402a6
[^c3]: detcore/src/syscalls/io.rs:4003-4018 sha256:601551fe5ad464fe

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-17 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
