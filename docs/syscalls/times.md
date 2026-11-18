---
schema: syscall-pages/v3
syscall: times
nr_x86_64: 100
related:
  - https://github.com/rrnewton/hermit/issues/498
  - https://github.com/rrnewton/hermit/issues/519
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+times&body=Discussion+for+the+%60times%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimes.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60times%60%2C+or+in+what+the+page+says%3F%0A
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
  reviewed_at: 2026-11-16
  based_on: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
---

## In short

Natively, both the elapsed-tick return and the CPU times come from the host. Hermit derives both from its logical clock and logical CPU accounting. Known gaps: 2.

## Determinism strategy

**What is nondeterministic natively.** `times` reports two host quantities at once: elapsed wall-clock ticks since some host-boot-related instant (the return value), and the calling process's user/system CPU times as the host scheduler accounted them, including reaped children's.[^linux] Both vary between runs and between hosts.

**What Hermit does.** Detcore determinizes both halves. The return value is elapsed ticks on the global logical clock, counted from the virtual boot instant plus the configured uptime offset, at the same `USER_HZ` tick rate Linux exposes, with the same wrapping conversion to `clock_t`.[^c1][^c2][^c4][^c5] The `tms` fields come from Detcore's per-process logical CPU accounting — user and syscall-system time aggregated across the process's threads — converted to ticks with Linux's truncation, never from host scheduler counters.[^c3][^c4][^c6]

**Interface fidelity.** A null `buf` stores nothing and still returns the elapsed ticks, as on Linux.[^c3] Child counters aggregate a child's totals into the parent when the child is reaped, and a forked process starts with fresh counters.[^c3][^c7] Tick conversion truncates rather than rounds, matching the kernel's conversion.[^c4]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] Every reported quantity is regenerated from logical time and logical CPU accounting at the same schedule point, so replay reproduces the same ticks without host input.[^c3]

## Known gaps

- G1 (by design): the elapsed-tick return value counts from the virtual boot instant (plus configured offset), so it is comparable between Hermit runs, never to a native run's host-boot-relative value.[^c5]
- G2 (by design): CPU times are logical accounting, not host scheduler time, so their ratio to elapsed time reflects executed work, not host wall-clock occupancy; a guest calibrating against a native ratio measures a different machine.[^c3][^c6]

## Discussion

[Start a discussion about `times`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+times&body=Discussion+for+the+%60times%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimes.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60times%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `times` (100) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1966`
- Dispatch: `detcore/src/lib.rs:8688`

### Tests

Tick projection: `detcore/src/syscalls/sysinfo.rs` (`logical_clock_ticks_include_boot_offset_and_fractional_seconds`, `logical_clock_ticks_wrap_configured_offset_like_linux_clock_t`) — what Hermit does, and interface fidelity.

### Citations

[^linux]: Linux `times(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/sys.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1966 sha256:d27b643c185ed892
[^c2]: detcore/src/lib.rs:8687-8688 sha256:e0b415f6635d038d
[^c3]: detcore/src/syscalls/sysinfo.rs:463-493 sha256:5dfe35d3a967053d
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/syscalls/sysinfo.rs#L463-L493
[^c4]: detcore/src/syscalls/sysinfo.rs:22-31 sha256:806ff51e57634dc0
[^c5]: detcore/src/syscalls/sysinfo.rs:55-64 sha256:60859f7af97540ec
[^c6]: detcore/src/tool_local.rs:7738-7748 sha256:61f13661ddf079b7
[^c7]: detcore/src/tool_local.rs:5716-5731 sha256:465998d86a303e3f

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-16 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-16 · rev 8e48c1c6605b · summary-first layout (In short, Known gaps, Appendix); claims unchanged.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
