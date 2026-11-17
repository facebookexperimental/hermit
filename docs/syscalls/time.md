---
schema: syscall-pages/v3
syscall: time
nr_x86_64: 201
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+time&body=Discussion+for+the+%60time%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60time%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
  detcore_class: determinized
  dispatch: "typed"
  refused: false
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

Natively, this returns the host wall clock in whole seconds. By default (time virtualization on), Hermit returns the virtual clock's whole seconds instead. Known gaps: 2.

## Determinism strategy

**What is nondeterministic natively.** `time` returns the host wall clock in whole seconds, so its result — and anything the guest seeds or orders by it — differs between runs and between hosts.[^linux]

**What Hermit does.** With time virtualization on (the deterministic configuration; `--no-virtualize-time` turns it off), Detcore determinizes the result: the handler reads the guest clock (the scheduler's logical time, observed as a global request and clamped per thread so it never goes backwards) and returns its whole seconds.[^c1][^c2][^c3][^c4] Without time virtualization the call passes through to the host clock.[^c5]

**Interface fidelity.** The seconds value is both the return value and, when `tloc` is non-null, stored through it; a null `tloc` stores nothing and is not an error.[^c3] A bad `tloc` pointer surfaces as the memory-write error rather than a fabricated result.[^c3] Sub-second information is truncated exactly as the interface specifies, so two reads within the same virtual second agree.[^c3]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] Virtualized readings regenerate from logical time at the same schedule point; without virtualization, a recording captures the host value the guest observed and replay returns it without reading the host clock.[^c5]

## Known gaps

- G1 (by design): the value is epoch-plus-logical-elapsed seconds, so it is comparable across Hermit runs with the same epoch and schedule, never to a native run's reading of the same instant.[^c3][^c4]
- G2 (unmeasured): no dedicated test runs this handler; the nearest tests exercise `time` only as the `gettimeofday` repair's store probe, not this call's own contract.[^c3]

## Discussion

[Start a discussion about `time`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+time&body=Discussion+for+the+%60time%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60time%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `time` (201) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1949`
- Dispatch: `detcore/src/lib.rs:7989`

### Tests

No dedicated test for this handler exists at this revision; that absence is G2. The handler itself is the evidence for the claims above pending one.[^c3]

### Citations

[^linux]: Linux `time(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/time.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1949 sha256:bab02b747529bee6
[^c2]: detcore/src/lib.rs:7988-7995 sha256:f319953eedd3775f
[^c3]: detcore/src/syscalls/time.rs:724-740 sha256:efe5c93a6f9cbe34
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/syscalls/time.rs#L724-L740
[^c4]: detcore/src/syscalls/time.rs:185-192 sha256:2bfaae47fe5e3eed
[^c5]: detcore/src/lib.rs:7978-7980 sha256:2018927e6bf82258

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-16 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-16 · rev 8e48c1c6605b · summary-first layout (In short, Known gaps, Appendix); claims unchanged.
