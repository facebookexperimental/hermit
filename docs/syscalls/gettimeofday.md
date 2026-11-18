---
schema: syscall-pages/v3
syscall: gettimeofday
nr_x86_64: 96
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+gettimeofday&body=Discussion+for+the+%60gettimeofday%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgettimeofday.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60gettimeofday%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, this returns the host wall clock. By default (time virtualization on), Hermit overwrites the time value with virtual time after running the call, while the timezone buffer still receives host state in run and record modes. Known gaps: 2.

## Determinism strategy

**What is nondeterministic natively.** `gettimeofday` returns the host wall clock at microsecond precision, so every result — and anything the guest derives from it, including ordering decisions and seeds — differs between runs.[^linux]

**What Hermit does.** With time virtualization on (the deterministic configuration; `--no-virtualize-time` turns it off), Detcore determinizes the result: the handler reads the guest clock (the scheduler's logical time, observed as a global request and clamped per thread so it never goes backwards) and stores that virtual time, at microsecond truncation, over whatever the forwarded call produced.[^c1][^c2][^c4][^c5] The handler deliberately still runs the call through the record/replay path first, then overwrites `tv`, so error behaviour and memory effects match the forwarded call while the value the guest reads is virtual.[^c4] Without time virtualization the call passes through to the host clock.[^c6]

**Interface fidelity.** A null `tv` stores nothing and is not an error, matching the forwarded call's result.[^c4] Fault behaviour is preserved word by word: when the forwarded call fails with `EFAULT`, a store probe establishes which words of `tv` the kernel actually reached, and only those are overwritten with virtual time; if that repair cannot be completed, the run fails closed rather than let host wall-clock words survive in guest memory.[^c4][^c11] Any other error leaves `tv` untouched, because only `EFAULT` says anything about what reached it.[^c4] The handler itself never writes the obsolete timezone argument — but the forwarded call it runs first does, so in run and record modes the guest's timezone buffer receives the host's timezone state unmodified.[^c4]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] Virtualized, the stored `tv` value is regenerated from logical time at the same schedule point; the overwrite-after-forward shape exists precisely so a recording captures the call as executed while the guest only ever reads virtual time from `tv`.[^c4][^c6] The timezone buffer gets no such treatment in any mode: the handler writes nothing there, so whatever the forwarded call wrote (host state in run and record modes) simply stands.[^c4] A replayed log's own `EFAULT` keeps the same repair discipline for `tv`.[^c4]

## Known gaps

- G1 (unimplemented): the timezone out-parameter is not virtualized. The forwarded call writes the host's timezone state into it in run and record modes, so host timezone configuration reaches the guest through this call even though the time value itself is virtual.[^c4]
- G2 (unmeasured): the repair path for a faulting `tv` depends on backend store probes; where a backend cannot prove which words were stored, the run fails closed instead of completing with a partially virtual value — safe, but stricter than Linux.[^c4]

## Discussion

[Start a discussion about `gettimeofday`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+gettimeofday&body=Discussion+for+the+%60gettimeofday%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgettimeofday.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60gettimeofday%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `gettimeofday` (96) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1730`
- Dispatch: `detcore/src/lib.rs:8233`

### Tests

Fault-repair proof obligations: `detcore/src/syscalls/time.rs` (`without_a_store_probe_the_repair_refuses`, `successful_time_store_probe_requires_an_overwrite`, `any_control_probe_error_means_the_efault_was_not_a_store_fault`) — interface fidelity, and record and replay.

### Citations

[^linux]: Linux `gettimeofday(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/time.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1730 sha256:310a3124777d80e5
[^c2]: detcore/src/lib.rs:8233-8239 sha256:c164fb54a01b22d1
[^c4]: detcore/src/syscalls/time.rs:685-722 sha256:a10e250f8324a803
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/syscalls/time.rs#L685-L722
[^c5]: detcore/src/syscalls/time.rs:185-192 sha256:2bfaae47fe5e3eed
[^c6]: detcore/src/lib.rs:8230-8232 sha256:2018927e6bf82258
[^c11]: detcore/src/syscalls/time.rs:292-345 sha256:bc3909d75a332b56

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-16 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-16 · rev 8e48c1c6605b · summary-first layout (In short, Known gaps, Appendix); claims unchanged.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
