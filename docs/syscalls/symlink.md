---
schema: syscall-pages/v2
syscall: symlink
nr_x86_64: 88
related:
  - https://github.com/rrnewton/hermit/issues/487
  - https://github.com/rrnewton/hermit/issues/489
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+symlink&body=Discussion+for+the+%60symlink%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsymlink.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60symlink%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
---

## Strategy

Detcore classifies `symlink` (88) as passthrough via passthrough dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: false.

- Reviewed list: `detcore/src/syscall_classification.rs:2074`

This facts-only page was generated from Detcore code facts. Checked analysis prose has not been added yet.

## Discussion

[Start a discussion about `symlink`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+symlink&body=Discussion+for+the+%60symlink%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsymlink.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60symlink%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
