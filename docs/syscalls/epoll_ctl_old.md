---
schema: syscall-pages/v2
syscall: epoll_ctl_old
nr_x86_64: 214
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_ctl_old&body=Discussion+for+the+%60epoll_ctl_old%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_ctl_old.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_ctl_old%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
  detcore_class: determinized
  dispatch: "typed"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
---

## Strategy

Detcore classifies `epoll_ctl_old` (214) as determinized-refused via typed dispatch; deterministic refusal: true; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1661`
- Dispatch: `detcore/src/lib.rs:8198`

This facts-only page was generated from Detcore code facts. Checked analysis prose has not been added yet.

## Discussion

[Start a discussion about `epoll_ctl_old`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_ctl_old&body=Discussion+for+the+%60epoll_ctl_old%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_ctl_old.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_ctl_old%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
