---
schema: syscall-pages/v2
syscall: mknod
nr_x86_64: 133
related:
  - https://github.com/rrnewton/hermit/issues/288
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mknod&body=Discussion+for+the+%60mknod%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmknod.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mknod%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 54bd03547df04c519fedfb6c97ede1933bda4182
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
---

## Strategy

Detcore classifies `mknod` (133) as passthrough via passthrough dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: false.

- Reviewed list: `detcore/src/syscall_classification.rs:2041`

This facts-only page was generated from Detcore code facts. Checked analysis prose has not been added yet.

## Discussion

[Start a discussion about `mknod`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mknod&body=Discussion+for+the+%60mknod%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmknod.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mknod%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
