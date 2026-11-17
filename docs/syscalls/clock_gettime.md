---
schema: syscall-pages/v3
syscall: clock_gettime
nr_x86_64: 228
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_gettime&body=Discussion+for+the+%60clock_gettime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_gettime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_gettime%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, every clock reads host time or host CPU accounting. By default (time virtualization on), Hermit answers from the guest's logical clock and logical CPU accounting, so readings repeat with the schedule. Known gaps: 2.

## Determinism strategy

**What is nondeterministic natively.** `clock_gettime` reads a host clock — wall time, a monotonic clock, or a process/thread CPU clock — so the returned timespec advances with real elapsed time and with how the host scheduler ran the process.[^linux] Two runs of the same program therefore observe different values everywhere a clock is read.

**What Hermit does.** With time virtualization on (the deterministic configuration; `--no-virtualize-time` turns it off), Detcore determinizes the call: typed dispatch reaches a handler that answers from the guest clock, the scheduler's logical time, observed as a global request so the read has a defined place in the schedule, and clamped per thread so no thread sees its own clock go backwards.[^c1][^c2][^c5][^c6] CPU clocks instead read Detcore's logical CPU accounting for the named process or thread, after the same guest-clock observation, so even a CPU-clock read keeps its schedule position.[^c4] Without time virtualization the call passes through to the host clock, and determinism then depends on record/replay capturing the observed value (next part).[^c7]

**Interface fidelity.** Clock ids are accepted or rejected as Linux decides from the id alone: the fixed clocks and `CLOCK_TAI` are known, the removed SGI cycle clock and ids from twelve up fail with `EINVAL`, and a negative id is decoded as a CPU-clock name whose validity depends on the task it names — except descriptor-encoded ids, which the classifier puts with the ordinary clocks.[^c3][^c8] A CPU-clock id naming no live task fails with `EINVAL`; an unknown fixed id fails before any clock observation is made.[^c4] A null output pointer fails with `EFAULT`, after the observation.[^c4] All non-CPU clocks are *coalesced*: realtime, monotonic, boottime, and the coarse variants all read the one guest clock.[^c4]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] Virtualized readings are regenerated when replay reaches the same call in the same schedule, because the value is a function of logical time there.[^c5] Without virtualization, a recording captures the host value the guest observed and replay returns it without reading the host clock — which is why every clock read of this family carries a record/replay handler rather than a plain passthrough in that configuration.[^c7]

## Known gaps

- G1 (by design): the offsets between the non-CPU clocks are not modelled — `CLOCK_TAI` has no leap-second offset from realtime here, and boottime includes nothing extra — because all of them read the one guest clock.[^c4]
- G2 (by design): a dynamic clock descriptor does not read the clock it names: the classifier treats descriptor ids as ordinary clocks, so the call succeeds with the guest clock's virtual time instead of the descriptor clock's reading.[^c8][^c4]

## Discussion

[Start a discussion about `clock_gettime`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_gettime&body=Discussion+for+the+%60clock_gettime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_gettime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_gettime%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `clock_gettime` (228) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1632`
- Dispatch: `detcore/src/lib.rs:7996`

### Tests

CPU-clock readings and dead-task ids: `detcore/src/cpu_clock/handler_tests.rs` (`cpu_clocks_read_the_logical_cpu_accounting`, `cpu_clock_ids_naming_no_task_answer_as_linux_does`) — what Hermit does, and interface fidelity. The shared clock-id table: `detcore/src/syscalls/time.rs` (`timer_create_clock_refusal_matches_linuxs_clock_table`) — interface fidelity.

### Citations

[^linux]: Linux `clock_gettime(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1632 sha256:8ab0a9216ecb32b8
[^c2]: detcore/src/lib.rs:7996-8002 sha256:6a297e311e55e02e
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L7996-L8002
[^c3]: detcore/src/syscalls/time.rs:55-66 sha256:ea09775213d38639
[^c4]: detcore/src/syscalls/time.rs:742-773 sha256:0af0a455c8cc2b74
[^c5]: detcore/src/syscalls/time.rs:185-192 sha256:2bfaae47fe5e3eed
[^c6]: detcore/src/tool_local.rs:5845-5850 sha256:fec2796a0addaa95
[^c7]: detcore/src/lib.rs:7978-7980 sha256:2018927e6bf82258
[^c8]: detcore/src/cpu_clock.rs:106-150 sha256:72ee848aa6b82421

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-16 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-16 · rev 8e48c1c6605b · summary-first layout (In short, Known gaps, Appendix); claims unchanged.
