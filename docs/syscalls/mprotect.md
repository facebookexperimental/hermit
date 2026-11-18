---
schema: syscall-pages/v3
syscall: mprotect
nr_x86_64: 10
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mprotect&body=Discussion+for+the+%60mprotect%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmprotect.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mprotect%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 403e350e9e82ee540529f097daccae6a60832d07
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
# prose: human/agent-authored, machine-checked (syscall-pages.rs check); v3 only
prose:
  status: checked
  reviewed_at: 2026-11-16
  based_on: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
  passthrough_risk: host-state
---

## In short

Natively, success depends partly on host policy (mount flags, LSM, deny-write-execute). Hermit passes the call through unchanged, so its answer is only as repeatable as that host configuration. Known gaps: 2.

## Determinism strategy

**What is nondeterministic natively.** Whether a protection change succeeds is partly host policy, not just guest state: the kernel checks the mapping's may-protect bits (cleared for file mappings by a `noexec` mount), the LSM `file_mprotect` hook, and memory-deny-write-execute state before applying it.[^linux] The same call on the same mapping can therefore succeed on one host configuration and fail with `EACCES` on another.

**What Hermit does.** Detcore passes `mprotect` through to the kernel unchanged, by classification rather than by per-call decision.[^c1][^c2] That makes the result exactly as deterministic as the host inputs above: within a fixed host configuration (mount flags, LSM policy, deny-write-execute state) and the guest's own modeled layout, the answer repeats; across differing host configurations it need not. This page is therefore triaged `host-state`, not `none`: Hermit does not model or mask those host-policy inputs for this call.[^c2]

**Interface fidelity.** Protection bits, alignment, length rounding, and the errnos are the kernel's own, because the kernel performs the change.[^c2][^linux] In particular the permission error may come from the mapping's may-protect bits, from LSM policy, or from deny-write-execute, indistinguishably to the guest — exactly as natively on that host.[^linux]

**Record & replay.** The call is subscribed in run mode but not in record/replay mode (code facts in the Appendix), so a replay executes it again against the replayed layout rather than consuming a recorded result.[^c2] That reproduces the recorded answer only while the host-policy inputs above are unchanged.

## Known gaps

- G1 (by design): host mount flags (`noexec`), LSM policy, and deny-write-execute state reach the guest's result unmasked; a run moved between differently configured hosts can change answers with no guest-visible cause inside Hermit.[^linux][^c2]
- G2 (unmeasured): kernel side effects beyond the protection change itself — VMA splitting and merging that a later fault or `/proc` maps read could observe — are the host kernel's, not separately modelled by Detcore for this call.[^c2]

## Discussion

[Start a discussion about `mprotect`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mprotect&body=Discussion+for+the+%60mprotect%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmprotect.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mprotect%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `mprotect` (10) as passthrough via passthrough dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: false.

- Reviewed list: `detcore/src/syscall_classification.rs:2048`

### Tests

The passthrough exercised against a real mapping (a helper re-protects a page so a later read faults): `detcore/tests/misc/mod.rs` (`random_read_before_an_unmapped_page`, via its helper `page_before_a_hole`) — what Hermit does. No dedicated `mprotect` policy test exists; that absence is the subject of G2.

### Citations

[^linux]: Linux `mprotect(2)` permission checks:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/mm/mprotect.c?h=v7.1.3#n937
[^c1]: detcore/src/syscall_classification.rs:2048 sha256:7467309d5c39fe53
[^c2]: detcore/src/lib.rs:8791 sha256:46fdc2c10422fe4b
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8791

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-16 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3, passthrough host-state); citations verified at this revision.
- 2026-11-16 · rev 8e48c1c6605b · summary-first layout (In short, Known gaps, Appendix); claims unchanged.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
