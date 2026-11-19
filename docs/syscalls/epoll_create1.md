---
schema: syscall-pages/v3
syscall: epoll_create1
nr_x86_64: 291
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_create1&body=Discussion+for+the+%60epoll_create1%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_create1.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_create1%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, the kernel picks the new descriptor number from the live descriptor table and can refuse when descriptor limits are reached. Hermit forwards the creation at a serialized point in the schedule and records the descriptor in its own table, so the number and later lookups repeat with the run. Known gaps: 3.

## Determinism strategy

**What is nondeterministic natively.** `epoll_create1` asks the kernel for a new epoll instance and a descriptor number for it, chosen from the calling thread's descriptor table as it stands at that instant, and the call can fail when the per-process descriptor-table limit has been reached.[^linux] Under concurrent descriptor activity the number a run receives, and whether creation succeeds at all, therefore depend on host scheduling and on descriptor use outside this call.

**What Hermit does.** Detcore determinizes creation by giving it a defined place in the serialized schedule and then forwarding it: typed dispatch reaches a handler that first files an empty resource request, runs the call through the record-or-replay path, and registers the returned descriptor in the descriptor table Detcore consults for later descriptor operations, marked as an epoll descriptor with its close-on-exec state carried from the creation flags.[^c1][^c2][^c3] The legacy `epoll_create` entry point reaches the same handler by conversion at dispatch, with no size argument left for the handler to judge, as named under Known gaps.[^c2]

**Interface fidelity.** The only creation flag with meaning is the close-on-exec flag, `EPOLL_CLOEXEC`, which shares its bit value with the corresponding open flag and is carried straight into the registered descriptor state.[^c3][^linux] Detcore checks no flag bits itself: a flags value the kernel rejects is rejected by the forwarded kernel call, and success or failure from descriptor limits is likewise the kernel answer Detcore forwards and registers only on success.[^c3]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] The handler always goes through the record-or-replay path and registers whatever descriptor number that path returns, so the descriptor-table entry is rebuilt on replay exactly where the recording created it.[^c3]

## Known gaps

- G1 (by design): the epoll instance itself is the host kernel object the forwarded call creates, not a Detcore model, and the descriptor number is the kernel number from that call; repeatability comes from serialized descriptor allocation and from replaying the creation, not from virtual descriptor numbering.[^c3]
- G2 (unmeasured): no dedicated unit test exercises this handler at this revision, and the integration guest credited in Tests exercises successful creation but not invalid-flag or descriptor-limit failure, so those forwarded-failure claims rest on the handler code alone pending such a test.[^c3]
- G3 (unimplemented): the legacy spelling's size argument is discarded by the conversion to this call, so a size of zero or less, which Linux refuses, instead creates an instance as a flags-free call here would; there is no fallback that restores the check.[^c2][^c3][^linux]

## Discussion

[Start a discussion about `epoll_create1`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_create1&body=Discussion+for+the+%60epoll_create1%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_create1.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_create1%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `epoll_create1` (291) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1658`
- Dispatch: `detcore/src/lib.rs:8436`

### Tests

Successful creation with the close-on-exec flag and descriptor-table use of the returned descriptor are exercised end to end by the guest program tests/c/epoll_determinism.c, run repeatedly for identical output by the integration test hermit-cli/tests/epoll_determinism.rs — what Hermit does, and interface fidelity. No dedicated unit test for this handler exists at this revision; that narrower absence is part of G2.[^c3]

### Citations

[^linux]: Linux `epoll_create1(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/eventpoll.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1658 sha256:9fc90e7d23855b26
[^c2]: detcore/src/lib.rs:8431-8436 sha256:0bc7d8f1e608e36c
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8431-L8436
[^c3]: detcore/src/syscalls/io.rs:3975-4001 sha256:83122eb266387b58

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-17 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
