---
schema: syscall-pages/v2
syscall: settimeofday
nr_x86_64: 164
related:
  - https://github.com/rrnewton/hermit/issues/460
  - https://github.com/rrnewton/hermit/issues/467
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+settimeofday&body=Discussion+for+the+%60settimeofday%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsettimeofday.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60settimeofday%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
  detcore_class: determinized
  dispatch: "pred:is_mount_ns_admin_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
---

## Strategy

Detcore classifies `settimeofday` (164) as determinized-refused via pred:is_mount_ns_admin_refused_syscall dispatch; deterministic refusal: true; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1922`
- Dispatch: `detcore/src/lib.rs:7608`

This facts-only page was generated from Detcore code facts. Checked analysis prose has not been added yet.

## Discussion

[Start a discussion about `settimeofday`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+settimeofday&body=Discussion+for+the+%60settimeofday%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsettimeofday.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60settimeofday%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
