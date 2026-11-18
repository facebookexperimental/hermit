---
schema: syscall-pages/v3
syscall: access
nr_x86_64: 21
related:
  - https://github.com/rrnewton/hermit/issues/1935
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+access&body=Discussion+for+the+%60access%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Faccess.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60access%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 403e350e9e82ee540529f097daccae6a60832d07
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
# prose: human/agent-authored, machine-checked (syscall-pages.rs check); v3 only
prose:
  status: checked
  reviewed_at: 2026-11-16
  based_on: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
  passthrough_risk: host-state
---

## In short

Natively, the answer comes from live host filesystem state and real credentials. Hermit forwards ordinary paths to the kernel and, outside record/replay modes, answers rendered-procfs paths itself. Known gaps: 2.

## Determinism strategy

**What is nondeterministic natively.** `access` answers from live host filesystem state — whether a path exists and whether the caller's real ids may read, write, or execute it — so the same guest gets different answers whenever the filesystem, mount view, or credentials around it differ.[^linux]

**What Hermit does.** Detcore passes `access` through to the kernel by classification for ordinary paths.[^c1][^c2] The exception is paths under the procfs surfaces Detcore renders itself: this legacy number is in the rendered-procfs path-call table, so outside record/replay modes Detcore answers for those paths from its rendered content instead of the host procfs.[^c4][^c5] For ordinary paths, passthrough is sound only under Hermit's standing host-environment contract: deterministic runs provide a stable filesystem and fixed credentials, so the kernel's answer is a function of that fixed environment rather than of anything that changes between repeated runs. Hermit does not make a changing filesystem deterministic, and this page's passthrough does not pretend otherwise. What Detcore adds around the passthrough is observation: on x86_64 the legacy number is on the record/replay subscription list, so the call cannot slip past the tool under the passthrough optimization.[^c3]

**Interface fidelity.** For ordinary paths, mode bits (`F_OK` and the read/write/execute checks) and their errnos are the kernel's own, evaluated against real (not effective) ids exactly as Linux specifies, because the kernel performs the check.[^c2][^linux] For rendered-procfs paths, Detcore's answer comes from its rendered content, checked with the capability set Linux checks for the call (the real-id set for this legacy number), through the same table its `faccessat` family uses.[^c4][^c5][^c7]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix), the x86_64 legacy number included by the architecture-specific subscription list.[^c3] The observed result is therefore part of the recorded execution rather than a fresh host filesystem read at replay time. One mode condition cuts the other way: rendered-procfs answering is skipped entirely when record/replay modes are configured, so in those modes even rendered paths take the ordinary forwarded path above.[^c6]

## Known gaps

- G1 (by design): the answer is exactly as stable as the provided filesystem and credentials; a file created, removed, or re-permissioned outside the guest between runs changes results without any Detcore involvement.[^c2]
- G2 (unmeasured): capability, ACL, and mount-flag corners of the permission check are whatever the host kernel computes; Detcore records no model of them for this call.[^c2]

## Discussion

[Start a discussion about `access`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+access&body=Discussion+for+the+%60access%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Faccess.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60access%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `access` (21) as passthrough via passthrough dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:2000`

### Tests

Delivery to the tool under the passthrough optimization: `detcore/src/lib.rs` (`passthru_opt_subscribes_every_determinized_syscall`) — record and replay. Rendered-procfs answering reaches this number: `detcore/src/syscalls/files/prefilter_tests.rs` (`a_representative_call_of_each_check_still_reaches_it`) — what Hermit does, and interface fidelity.

### Citations

[^linux]: Linux `access(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/open.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:2000 sha256:338d98bf9b74cc53
[^c2]: detcore/src/lib.rs:8791 sha256:46fdc2c10422fe4b
[^c3]: detcore/src/lib.rs:5394-5401 sha256:b70894cae0b286e2
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L5394-L5401
[^c4]: detcore/src/syscalls/files/rendered_procfs.rs:8372-8406 sha256:0623ae321077732f
[^c5]: detcore/src/syscalls/files/prefilter_tests.rs:290-306 sha256:501d2754f4fd2903
[^c6]: detcore/src/syscalls/files/rendered_procfs.rs:7765-7780 sha256:3b4d70911051ee26
[^c7]: detcore/src/syscalls/files/rendered_procfs.rs:8077-8093 sha256:adb87316238537e6

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-16 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-16 · rev 8e48c1c6605b · summary-first layout (In short, Known gaps, Appendix); claims unchanged.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
