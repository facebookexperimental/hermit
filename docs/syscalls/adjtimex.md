---
schema: syscall-pages/v3
syscall: adjtimex
nr_x86_64: 159
related:
  - https://github.com/rrnewton/hermit/issues/333
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+adjtimex&body=Discussion+for+the+%60adjtimex%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fadjtimex.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60adjtimex%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, this reads and steers the host NTP discipline. By default (time virtualization on), Hermit answers queries with a fixed unsynchronized snapshot on virtual time and refuses steering. Known gaps: 2.

## Determinism strategy

**What is nondeterministic natively.** `adjtimex` both reads and steers the host kernel's NTP discipline: a query returns host synchronization state (status, frequency and offset estimates, tick length, current time), and the adjustment modes mutate the host clock that every other process observes.[^linux]

**What Hermit does.** With time virtualization on (the deterministic configuration; `--no-virtualize-time` turns it off), Detcore determinizes the query and refuses the steering: a query gets a fixed unsynchronized discipline snapshot whose only varying field is the current virtual time, while any adjustment mode fails with `EPERM`, as a capability-gated host mutation the guest is never granted.[^c1][^c2][^c4] Without time virtualization the call takes the unsupported-syscall policy: a fail-closed run ends there, while a permissive run reports the call and passes it through, reaching the host discipline after all.[^c2][^c5]

**Interface fidelity.** Exactly two modes are queries and honored: no mode bits, and `ADJ_OFFSET_SS_READ` alone; anything else — including a query bit combined with adjustment bits — is refused with `EPERM` before the buffer is written.[^c3][^c4] A null buffer fails with `EFAULT`.[^c4] The snapshot sets the unsynchronized status bit and the nominal tick, truncates virtual time to microseconds, zeroes every other discipline field, and returns the kernel's unsynchronized return code.[^c3][^c4]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] The snapshot is regenerated from virtual time at the same schedule point; the refused modes refuse identically.[^c4]

## Known gaps

- G1 (by design): no NTP discipline is modelled — frequency, offset, and error estimates are always the fixed snapshot — so a guest measuring synchronization quality sees a permanently unsynchronized clock.[^c3]
- G2 (by design): clock steering is refused even for a guest holding the time capability, because the virtual clock is owned by the run's epoch configuration, not by the guest.[^c4]

## Discussion

[Start a discussion about `adjtimex`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+adjtimex&body=Discussion+for+the+%60adjtimex%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fadjtimex.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60adjtimex%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `adjtimex` (159) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1614`
- Dispatch: `detcore/src/lib.rs:8021`

### Tests

Query/mutation boundary and snapshot: `detcore/src/syscalls/time.rs` (`timex_policy_distinguishes_queries_from_mutations`, `timex_snapshot_is_unsynchronized_and_uses_virtual_time`) — interface fidelity, and what Hermit does.

### Citations

[^linux]: Linux `adjtimex(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/ntp.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1614 sha256:83559dce6281bf2d
[^c2]: detcore/src/lib.rs:8021-8032 sha256:92c9c3b78daa6c6d
[^c3]: detcore/src/syscalls/time.rs:162-181 sha256:9d53c5f9c9cd0cdf
[^c4]: detcore/src/syscalls/time.rs:817-858 sha256:fbcdb3caabb0543b
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/syscalls/time.rs#L817-L858
[^c5]: detcore/src/lib.rs:1898-1907 sha256:72acd2655164229d

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-16 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-16 · rev 8e48c1c6605b · summary-first layout (In short, Known gaps, Appendix); claims unchanged.
