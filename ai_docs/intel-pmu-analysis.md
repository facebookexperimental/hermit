# Intel PMU parity analysis

Date: 2026-07-21

Analyzed revisions:

- Reverie `075d1eff799eb619282cedd303afe9fdacea02a5`
- Hermit `592d5c6ccbced0d1240b6562ff87652cb706f142`
- rr `39e5c18e7e43236b7ca0fb1eb647fe9c93e3934e`
- Intel perfmon `6e3329d20457aad11d8cc323b85aa6a16b075918`
- Linux `248951ddc14de84de3910f9b13f51491a8cd91df`

## Conclusion

Intel support is not at parity across the CPUs that Reverie claims to support.
The legacy Intel Core path through Comet Lake uses the expected retired
conditional-branch event and the same 100-RCB margin as rr. Newer Intel and
hybrid Intel have correctness gaps:

1. Ice Lake through Meteor Lake-era P-cores need raw event `0x5111c4`
   for all retired conditional branches. Reverie uses `0x5101c4`, which
   counts only taken conditional branches on these CPUs.
2. Alder Lake E-cores need `0x517ec4` and the `cpu_atom` PMU's dynamic event
   type. Reverie has one global config, always uses `PERF_TYPE_RAW`, and cannot
   distinguish P-cores from E-cores.
3. Hermit pins deterministic-preemption runs before PMU initialization, so a
   tracee does not migrate between PMU types. However, it chooses a random core
   and may select an E-core that Reverie configures as an Alder Lake P-core.
4. Intel's 100/125-RCB margins match rr's table, but Reverie has no skid-bound
   test. The values cannot be certified for the currently wrong events or for
   E-cores.
5. `precise_ip=1` is supported by Intel's intended conditional-branch events,
   but the CPUID Debug Store bit is only a coarse gate. Reverie does not fall
   back if the kernel rejects precise sampling, and validation failures only
   produce a warning.

The practical status is:

| Platform | Status |
| --- | --- |
| Nehalem through Comet Lake Core CPUs in the allowlist | Mostly supported; event and rr margin agree, but there is no current Intel skid evidence. |
| Ice Lake, Tiger Lake, Sapphire Rapids | Partial/incorrect; the configured event counts taken branches, not all conditional branches. Model coverage is also incomplete. |
| Alder Lake P-core | Partial/incorrect; same event mismatch, although the 125-RCB rr margin is represented. |
| Alder Lake E-core | Not supported correctly; wrong event encoding and no hybrid PMU event type. |
| Raptor Lake and newer hybrid Intel | Rejected as unknown or misconfigured; the model table has not kept pace with rr. |
| AMD Zen 1-5 | Broad family match and correct `0x5100d1` event, plus the AMD-specific SpecLockMap check; 10,000-RCB margin follows rr's unbounded-skid policy. |

No Intel machine was available for this investigation. The findings above are
from code inspection, Intel's official event data, Linux hybrid-PMU behavior,
and current rr.

## Current Reverie design

`PmuConfig` is a process-global `LazyLock`. On x86 it reads CPUID family/model
once and chooses an event and skid margin
(`reverie-ptrace/src/timer.rs:62-132`). It currently maps:

- Intel family 6 allowlisted models: `0x5101c4`, margin 100, except Alder Lake
  and Sapphire Rapids at 125.
- AMD families `0x17`, `0x19`, and `0x1a`: `0x5100d1`, margin 10,000.

The timer uses two pinned perf counters for the tracee
(`reverie-ptrace/src/timer.rs:533-555`):

- a sampling counter that raises the asynchronous signal;
- a continuously enabled counting counter used as the RCB clock.

For precise requests, the signal is programmed `skid_margin` RCBs early and
Reverie single-steps to the clock target
(`reverie-ptrace/src/timer.rs:570-604`). A margin that is too small can put the
clock beyond the target and trigger the assertion in precise delivery.

`Builder::create` uses `PERF_TYPE_RAW`, `cpu=-1`, `pinned=1`, and excludes
kernel/guest/hypervisor events (`reverie-ptrace/src/perf.rs:190-220`). This is
reasonable for homogeneous x86, but it does not express which hybrid PMU owns a
raw event.

## Event-code parity

### Legacy Intel

Reverie's `0x5101c4` agrees with current rr for Nehalem through Comet Lake. On
those microarchitectures the `0x01` umask represents the conditional-retired
branch event used as the deterministic clock.

### Ice Lake and newer Intel Core PMUs

Current rr uses:

- `0x5111c4` for Ice Lake, Tiger Lake, Rocket Lake, Alder Lake P-cores,
  Raptor Lake P-cores, Sapphire Rapids, Emerald Rapids, Granite Rapids, and
  Meteor Lake;
- 125-RCB skid for Alder Lake and most newer Core products, 100 for Ice/Tiger;
- newer extended configs for Lunar Lake and Arrow Lake.

Intel's official event files confirm why `0x5111c4` is required. On Ice Lake,
Alder Lake Golden Cove, and Sapphire Rapids:

| Event | Event code | Umask |
| --- | --- | --- |
| `BR_INST_RETIRED.COND_TAKEN` | `0xc4` | `0x01` |
| `BR_INST_RETIRED.COND_NTAKEN` | `0xc4` | `0x10` |
| `BR_INST_RETIRED.COND` | `0xc4` | `0x11` |

Therefore Reverie's `0x5101c4` is a valid counter, but it has the wrong
semantics on these CPUs: it omits not-taken conditional branches. This is more
dangerous than a `perf_event_open` failure because the counter can appear
healthy.

The current validation cannot detect that error. `do_branches(500)` is a
countdown loop whose `jnz` is taken for almost every iteration
(`reverie-ptrace/src/perf.rs:558-575`), and `check_working_counters` only checks
that at least 500 events were seen (`reverie-ptrace/src/validation.rs:296-317`).
Setup/teardown branches supply enough additional taken events for the
taken-only counter to pass.

### Alder Lake hybrid PMUs

Intel documents different events for the two core types:

| Core type | PMU | `BR_INST_RETIRED.COND` |
| --- | --- | --- |
| Golden Cove P-core | `cpu_core` | event `0xc4`, umask `0x11` (`0x5111c4`) |
| Gracemont E-core | `cpu_atom` | event `0xc4`, umask `0x7e` (`0x517ec4`) |

Linux exports `/sys/bus/event_source/devices/cpu_core` and `cpu_atom`, each with
its own `cpus` and dynamic `type` files. Raw events must use the owning PMU's
type. Current rr enumerates those groups, selects a per-CPU PMU config and event
type, validates each PMU while pinned to it, refuses unbound operation with
multiple PMUs, and now prefers P-cores by default.

Reverie does none of that. CPUID leaf 1 model `0x9a` identifies the overall
Alder Lake product but not the selected core type, and `PmuConfig` has no event
type field. Even a corrected P-core umask would leave E-core execution wrong.

Hermit does mitigate migration. When deterministic preemption is enabled it
sets `pin_threads`, chooses one random logical CPU, and applies affinity before
the container closure starts (`hermit-cli/src/bin/hermit/container.rs:16-29`,
`reverie-process/src/container.rs:664-668`). Thus PMU CPUID probing and tracee
execution occur on one core. The remaining problem is choosing the correct
configuration for that pinned core. A short-term P-core-only policy would be
safer than random selection; the complete solution is per-core PMU discovery.

### Model coverage

The Intel allowlist is stale. Examples in current rr but absent from Reverie
include Ice Lake client/server IDs, Tiger Lake model `0x8c`, Rocket Lake,
Alder Lake model `0x97`, all Raptor Lake IDs, Emerald/Granite Rapids, Meteor
Lake, Lunar Lake, Arrow Lake, and Atom-derived E-core-only products. Reverie's
model `0x86` label also says Ice Lake while Intel's map identifies that ID as
Snow Ridge.

Unknown models panic during `PmuConfig` construction. Failing closed is better
than silently choosing an event, but the resulting support coverage is much
narrower than current Intel hardware.

## `precise_ip` assessment

`has_precise_ip` returns CPUID leaf 1 `DS` (Debug Store)
(`reverie-ptrace/src/timer.rs:163-175`). When true, only the sampling counter
gets `precise_ip=1`; the clock counter remains non-precise. Intel's official
Ice Lake, Alder Lake P/E, and Sapphire Rapids definitions mark the intended
`BR_INST_RETIRED.COND` events `Precise: 1` and list PEBS-capable counters, so
using precise level 1 is valid with the correct event and PMU.

There are still two weaknesses:

1. `DS` does not prove that this raw event, PMU type, kernel, VM, and permission
   setup accepts precise sampling. The authoritative probe is
   `perf_event_open` with the final attributes.
2. PMU self-validation runs with and without precise mode, but
   `Builder::check_for_pmu_bugs` only logs a warning
   (`reverie-ptrace/src/perf.rs:249-253`). It does not disable precise mode or
   stop timer creation. If final counter creation fails, `Timer::new` unwraps
   the error (`reverie-ptrace/src/timer.rs:224-235`) and panics.

The perf wrapper itself correctly notes that precise sampling reduces sample-IP
skid, not necessarily asynchronous notification skid, and can affect counts
(`reverie-ptrace/src/perf.rs:175-184`). `precise_ip` should therefore remain a
runtime-validated optimization, not evidence that a 100/125-RCB notification
margin is safe.

## Skid-margin assessment

The configured Intel values match current rr:

- 100 RCB for the legacy Core generations and Ice/Tiger;
- 125 RCB for Alder Lake P-cores and Sapphire Rapids;
- current rr uses 100 RCB for Gracemont-family E-cores.

This is a sound source for defaults, but Reverie does not measure or enforce the
assumption at startup. Its checks cover counter creation, counter progress, the
period ioctl, and the Intel KVM `IN_TXCP` bug. They do not measure notification
skid or run a tail distribution. The 2025 PMU refactor adopted rr's 100/125
values while retaining Reverie's `precise_ip=1` behavior.

The new `pmu_skid` tool can provide the missing evidence, but its initial
vendor-only Intel mapping also uses `0x5101c4`. It must share the corrected
model/core-type mapping before its results are meaningful for Ice Lake or
hybrid Intel. Measurements should be run separately on each supported P- and
E-core PMU, with at least 1,000 iterations and representative host load. The
AMD EPYC runs already showed that p99 can look tight while rare maxima are much
larger; Intel certification should not rely on a handful of samples.

Verdict: retain 100/125 as provisional rr-derived defaults. Do not lower them.
Do not claim them verified until the event mapping is corrected and the
measurement tool runs on actual Intel models. If measured maxima exceed the
margin, precise timer delivery can overshoot its target and the margin must be
raised per PMU/core type.

## Validation and CI gaps

Reverie's generic PMU checks are useful but incomplete:

- Both normal and precise modes test counter creation/progress and
  `PERF_EVENT_IOC_PERIOD`.
- AMD additionally tests SpecLockMap configuration.
- Intel additionally tests the KVM `IN_TXCP` issue.
- None tests taken and not-taken conditional branches separately.
- None measures overflow-notification skid.
- None enumerates or validates every hybrid PMU/core type.
- Validation errors are warning-only at the builder hook.

GitHub-hosted regular jobs commonly lack PMU permission, and PMU tests return
early when the generic instruction counter cannot be opened. Self-hosted jobs
run host-dependent tests, but workflow labels do not specify CPU vendor/model
and the workflows do not print or assert it. The available host for this investigation is AMD EPYC 9D85, and there is no
enforceable Intel coverage lane.

## Recommended work

1. **Correct event semantics first.** Bring Reverie's Intel model/event table up
   to current rr and Intel perfmon data: legacy Core `0x5101c4`, newer Core
   `0x5111c4`, and Atom/E-core `0x517ec4`, including current model IDs.
2. **Make configuration core-aware.** Pass the pinned CPU into PMU setup or read
   `sched_getcpu()` after affinity, discover `cpu_core`/`cpu_atom` membership
   and dynamic PMU type from sysfs, and store event type in `PmuConfig`.
3. **Prefer P-cores as an incremental mitigation.** Until E-core support is
   implemented and tested, choose from `cpu_core/cpus` rather than all logical
   CPUs when deterministic preemption is enabled. Fail clearly if no supported
   PMU is available.
4. **Strengthen semantic validation.** Add isolated taken-only and not-taken-only
   branch sequences and compare exact deltas. Run validation on each supported
   hybrid PMU while pinned to that PMU.
5. **Make capability failures actionable.** Treat wrong-event/zero-count checks
   as fatal for deterministic preemption. If `precise_ip=1` fails but level 0
   works, log the fallback and select a separately measured non-precise margin.
6. **Unify the benchmark mapping.** Share PMU selection code with `pmu_skid` or
   emit the resolved event type/config from Reverie so the diagnostic cannot
   silently benchmark a different event.
7. **Add an Intel hardware lane.** Record vendor/family/model/core type in CI and
   require at least one modern Core PMU plus one hybrid E-core run. Archive the
   benchmark distribution with the job.

## Sources

Internal code:

- `reverie-ptrace/src/timer.rs:62-175, 533-604, 741-764`
- `reverie-ptrace/src/perf.rs:175-220, 249-253, 501-575`
- `reverie-ptrace/src/validation.rs:80-167, 296-351, 428-503`
- `hermit-cli/src/bin/hermit/container.rs:16-29`
- `hermit-cli/src/bin/hermit/run.rs:80-89, 617-649`
- `reverie-process/src/container.rs:561-568, 664-668`

External references:

- [rr PMU table](https://github.com/rr-debugger/rr/blob/39e5c18e7e43236b7ca0fb1eb647fe9c93e3934e/src/PerfCounters.cc#L202-L255)
- [rr hybrid PMU discovery](https://github.com/rr-debugger/rr/blob/39e5c18e7e43236b7ca0fb1eb647fe9c93e3934e/src/PerfCounters_x86.h#L176-L258)
- [rr hybrid-core fix](https://github.com/rr-debugger/rr/commit/739c0d9bc6f9)
- [rr P-core preference](https://github.com/rr-debugger/rr/commit/bbe8772270df)
- [Linux Intel hybrid PMU documentation](https://github.com/torvalds/linux/blob/248951ddc14de84de3910f9b13f51491a8cd91df/tools/perf/Documentation/intel-hybrid.txt)
- [Intel Ice Lake events](https://github.com/intel/perfmon/blob/6e3329d20457aad11d8cc323b85aa6a16b075918/ICL/events/icelake_core.json)
- [Intel Alder Lake P-core events](https://github.com/intel/perfmon/blob/6e3329d20457aad11d8cc323b85aa6a16b075918/ADL/events/alderlake_goldencove_core.json)
- [Intel Alder Lake E-core events](https://github.com/intel/perfmon/blob/6e3329d20457aad11d8cc323b85aa6a16b075918/ADL/events/alderlake_gracemont_core.json)
- [Intel Sapphire Rapids events](https://github.com/intel/perfmon/blob/6e3329d20457aad11d8cc323b85aa6a16b075918/SPR/events/sapphirerapids_core.json)
- Reverie commit `3e1feb31dbad3ae8e70d5167ea91133ee78b10df`
  (`Refactor PMU configuration`, 2025-01-30)
