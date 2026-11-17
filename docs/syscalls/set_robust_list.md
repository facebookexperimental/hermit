---
schema: syscall-pages/v2
syscall: set_robust_list
nr_x86_64: 273
related:
  - https://github.com/rrnewton/hermit/issues/1953
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+set_robust_list&body=Discussion+for+the+%60set_robust_list%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fset_robust_list.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60set_robust_list%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
  detcore_class: passthrough
  dispatch: "sysno-guard"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
---

## Strategy

Detcore classifies `set_robust_list` (273) as passthrough via sysno-guard dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:2067`
- Dispatch: `detcore/src/lib.rs:8502`

This facts-only page was generated from Detcore code facts. Checked analysis prose has not been added yet.

## Discussion

[Start a discussion about `set_robust_list`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+set_robust_list&body=Discussion+for+the+%60set_robust_list%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fset_robust_list.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60set_robust_list%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
