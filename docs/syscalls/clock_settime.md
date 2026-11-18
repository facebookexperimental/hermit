---
schema: syscall-pages/v3
syscall: clock_settime
nr_x86_64: 227
related:
  - https://github.com/rrnewton/hermit/issues/340
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_settime&body=Discussion+for+the+%60clock_settime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_settime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_settime%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 403e350e9e82ee540529f097daccae6a60832d07
  detcore_class: determinized
  dispatch: "typed"
  refused: true
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

Natively, this sets the host wall clock for everyone. Hermit refuses it outright; the virtual epoch is run configuration, not guest-writable. Known gaps: 1.

## Determinism strategy

**What is nondeterministic natively.** `clock_settime` does not read nondeterminism, it creates it: setting the host wall clock changes what every later clock read on the machine returns, for this process and all others.[^linux]

**What Hermit does.** Detcore refuses the call with `EPERM` at dispatch, unconditionally: the number is in the deterministic-refusal set, and the arm discards its arguments and returns the error without touching any clock.[^c1][^c2][^c3][^c4] The virtual clock's epoch is run configuration, chosen by whoever launched the run; a guest cannot move it from inside, which is what makes later clock reads reproducible.

**Interface fidelity.** The refusal precedes all other validation: an unknown clock id and a null timespec pointer get the same `EPERM`, with no earlier argument-specific answer, because the arm never reads its arguments.[^c3] A privileged guest is refused identically; there is no capability the guest can hold that changes the answer.[^c3]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c3] The result is a constant (`EPERM`), so replay reproduces it exactly and no clock state is recorded.[^c3]

## Known gaps

- G1 (by design): there is no in-guest way to move the virtual clock through this call; guests that test clock-setting behaviour (time daemons, installers) see a permanently unprivileged answer, and the only way to run at a different epoch is to relaunch with one.[^c3]

## Discussion

[Start a discussion about `clock_settime`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_settime&body=Discussion+for+the+%60clock_settime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_settime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_settime%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `clock_settime` (227) as determinized-refused via typed dispatch; deterministic refusal: true; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1638`
- Dispatch: `detcore/src/lib.rs:8264`

### Tests

The fixed-error refusal aggregate this call belongs to: `detcore/src/syscall_classification.rs` (`deterministic_refusal_aggregate_includes_fixed_error_families`) — what Hermit does.

### Citations

[^linux]: Linux `clock_settime(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1638 sha256:43b78c7486dd3208
[^c2]: detcore/src/syscall_classification.rs:1555-1564 sha256:d6fb9993d44cbe48
[^c3]: detcore/src/lib.rs:8264-8266 sha256:ce80d6b93fe56ba9
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8264-L8266
[^c4]: detcore/src/lib.rs:678-679 sha256:dfe85e842e0e8a89

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-16 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-16 · rev 8e48c1c6605b · summary-first layout (In short, Known gaps, Appendix); claims unchanged.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
