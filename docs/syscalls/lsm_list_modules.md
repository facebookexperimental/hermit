---
schema: syscall-pages/v2
syscall: lsm_list_modules
nr_x86_64: 461
related:
  - https://github.com/rrnewton/hermit/issues/639
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lsm_list_modules&body=Discussion+for+the+%60lsm_list_modules%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flsm_list_modules.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lsm_list_modules%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 54bd03547df04c519fedfb6c97ede1933bda4182
  detcore_class: determinized
  dispatch: "pred:is_host_kernel_probe_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
---

## Strategy

Detcore classifies `lsm_list_modules` (461) as determinized-refused via pred:is_host_kernel_probe_syscall dispatch; deterministic refusal: true; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1771`
- Dispatch: `detcore/src/lib.rs:7702`

This facts-only page was generated from Detcore code facts. Checked analysis prose has not been added yet.

## Discussion

[Start a discussion about `lsm_list_modules`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lsm_list_modules&body=Discussion+for+the+%60lsm_list_modules%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flsm_list_modules.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lsm_list_modules%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
