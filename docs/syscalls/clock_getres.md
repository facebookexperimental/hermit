---
schema: syscall-pages/v3
syscall: clock_getres
nr_x86_64: 229
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_getres&body=Discussion+for+the+%60clock_getres%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_getres.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_getres%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, the reported resolution is a property of the host clocksource. By default (time virtualization on), Hermit reports one fixed resolution for ordinary clocks, so a guest that sizes its timing loops from this answer sizes them for the virtual clock. Known gaps: 1.

## Determinism strategy

**What is nondeterministic natively.** The resolution a host reports depends on the host's clocksource and kernel configuration, and guests use it to decide how finely they can measure — so the *value itself* is a small host fingerprint, and behaviour derived from it can differ between hosts.[^linux]

**What Hermit does.** With time virtualization on (the deterministic configuration; `--no-virtualize-time` turns it off), Detcore determinizes the answer: every non-CPU clock reports one fixed resolution, a constant ten microseconds, whatever the host clocksource could do.[^c1][^c2][^c4] A CPU clock reports its kind's resolution from Detcore's logical CPU accounting, after the named task is validated the way Linux validates it.[^c4] Without time virtualization the call passes through to the host, with determinism resting on record/replay capturing the observed value.[^c5]

**Interface fidelity.** Clock ids are accepted or rejected as Linux decides from the id alone: the fixed clocks and `CLOCK_TAI` are known, the removed SGI cycle clock and ids from twelve up fail with `EINVAL`, and a negative id is decoded as a CPU-clock name validated against the task it names — except descriptor-encoded ids, which classify as ordinary clocks and so report the same fixed resolution.[^c3][^c4][^c6] A null `res` pointer is valid, as on Linux: the id is validated, zero is returned, and nothing is stored.[^c4]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] The virtualized answer is a constant (or a CPU-kind constant), so replay regenerates it; the non-virtualized answer is captured by the recording like the other clock reads.[^c5]

## Known gaps

- G1 (by design): the fixed ten-microsecond resolution replaces the per-clock resolutions a modern host kernel reports (often a single nanosecond), so a guest that sizes its timing loops from this answer sizes them for the virtual clock, not the host.[^c4]

## Discussion

[Start a discussion about `clock_getres`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_getres&body=Discussion+for+the+%60clock_getres%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_getres.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_getres%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `clock_getres` (229) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1629`
- Dispatch: `detcore/src/lib.rs:8003`

### Tests

CPU-clock task validation through this call: `detcore/src/cpu_clock/handler_tests.rs` (`clock_getres_validates_the_task_a_cpu_clock_names`) — interface fidelity. The shared clock-id table: `detcore/src/syscalls/time.rs` (`timer_create_clock_refusal_matches_linuxs_clock_table`) — interface fidelity.

### Citations

[^linux]: Linux `clock_getres(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1629 sha256:00b8308a0ee86a95
[^c2]: detcore/src/lib.rs:8003-8009 sha256:47575a8e7cdfd456
[^c3]: detcore/src/syscalls/time.rs:55-66 sha256:ea09775213d38639
[^c4]: detcore/src/syscalls/time.rs:775-815 sha256:f229250e93cd7931
[^c5]: detcore/src/lib.rs:7978-7980 sha256:2018927e6bf82258
[^c6]: detcore/src/cpu_clock.rs:106-150 sha256:72ee848aa6b82421

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-16 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-16 · rev 8e48c1c6605b · summary-first layout (In short, Known gaps, Appendix); claims unchanged.
