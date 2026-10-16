# Preemption support

`--max-timeslice` promises to regain control from guest code even if it makes
no system call. A logical clock, guest POSIX timer, or scheduling decision at a
system call does not supply that mechanism.

Hermit's launch layer resolves an omitted maximum from
`BackendCapabilities::delivers_preemption_timer_events`: a runtime without the
mechanism gets a disabled default. An explicit positive maximum, including
`--max-timeslice=200000000`, is refused before guest startup on DBT, SaBRe, KVM,
LiteInst and in-guest-trap. Config-file and `--from` values are explicit inputs
too; they retain the same precedence as command-line values. Explicit disabled
or zero maximums remain accepted. Ptrace and e9patch preprocessing keep their
existing timer behavior.

The refusal is `unsupported_preemption: --backend=NAME refuses FLAG: this
runtime cannot deliver Detcore preemption timer events; the guest has not
started`, classified as `HERMIT_POLICY_REFUSAL` (exit 122). Timer-specific
controls listed below are refused even when the maximum is disabled. An active
per-thread chaos slowdown and per-thread preemption-point replay are included;
ordinary chaos and schedule-event recording/replay are separate.

The resolved value is used by Detcore, rendered by the run options and verbose
controller diagnostics (`--log=debug` or `--verify-verbose`), saved in
`--save-config` and run manifests, and reported in the run summary's
`preemption.max_timeslice_ns` (null means disabled). An absent `preemption`
object in an older summary means unknown. This configuration metadata appears
in the full human summary; it does not alter the existing INFO guest-behavior
comparison.

DBT's existing CLI summary reports native counters separately; it prints the
same effective-maximum metadata. This change does not add a DBT CLI
`--summary-json` producer where none existed.

Library launch APIs receive an effective `DetConfig`, without command-line
provenance. They never guess whether a positive value was inherited: they
refuse it on timerless backends. Direct callers requesting no preemption pass
`max_timeslice: None`.

The source inventory below is at Hermit
`532527297e278c30095427ea61e7fd23c5065b2f`, with Reverie
`75575dc018555cc08531b2b2204045a6fc9cfbbb`. Line numbers refer to those revisions;
they describe implementation evidence, not measured parity.

| Execution path | What happens to a requested maximum | Source |
| --- | --- | --- |
| ptrace | Arms a retired-conditional-branch timer. An unavailable host counter/profile produces a warning and disables the maximum in the CLI. | Reverie `reverie-ptrace/src/task.rs:9111`; Hermit `hermit-cli/src/bin/hermit/run.rs:6652` |
| DBT | Replaces the maximum with `None`. The underlying timer methods return `ENOSYS`. | Hermit `detcore-dbt/src/lib.rs:460`; Reverie `reverie-dbt/src/lib.rs:751` |
| LiteInst and in-guest-trap | The CLI and library already refuse a maximum before launching the guest. The shared in-guest runtime cannot arm the timer. | Hermit `hermit-cli/src/bin/hermit/run.rs:5676`, `hermit-cli/src/lib.rs:1119`; Reverie `reverie-inguest/src/guest/host.rs:3348` |
| SaBRe | Timer methods return success without arming a timer. | Reverie `experimental/reverie-sabre/src/reverie_adapter.rs:1482` |
| KVM | Timer methods return success without arming a timer. The backend capability correctly reports no preemption timer delivery. | Reverie `reverie-kvm/src/runtime.rs:1789`, `reverie/src/capabilities.rs:323` |
| Hermit e9patch preprocessing | Executes through ptrace after rewriting; uses ptrace's timer. | Hermit `hermit-cli/src/bin/hermit/run.rs:5506` |
| Direct Reverie e9patch Tool host | Timer methods return an Unsupported error. This is not the execution path used by Hermit's e9patch CLI option. | Reverie `reverie-e9patch/src/tool_host.rs:556`; Hermit `hermit-cli/src/lib.rs:2344` |

The parser default maximum is 200,000,000 virtual nanoseconds
(`detcore-model/src/config.rs:577`). `--max-timeslice=disabled` and
`--max-timeslice=0` explicitly request no maximum. `--preemption-timeout` is an
alias. `--no-rcb-time` changes the logical clock; it does **not** disable an
active maximum.

## Other settings

| Setting | Relevant behavior |
| --- | --- |
| `--imprecise-timers` | Selects an imprecise rather than precise timer arm (`detcore/src/lib.rs:1558`). |
| `--interrupt-at` | Requires an armed timer and RCB logical time to stop at the requested branch count (`detcore/src/lib.rs:1514`). |
| `--preemption-stacktrace`, `--preemption-stacktrace-log-file` | Produce their requested output only in the timer-event handler (`detcore/src/lib.rs:3073`). |
| `--panic-on-rbc-overshoot` (alias `--panic-on-rcb-overshoot`) | Checks a precise timer's branch-count overshoot (`detcore/src/lib.rs:871`). |
| `--skid-margin` | Configures the ptrace PMU timer, not a guest clock (`hermit-cli/src/bin/hermit/run.rs:6414`). |
| `--replay-preemptions-from` | Loads per-thread preemption points. With both target and maximum absent, the thread state clears its deadlines without enforcing those points (`detcore/src/tool_local.rs:5394`). |
| `--chaos-per-thread-slowdown` | Its RCB-time slowdown needs RCB logical time; ordinary chaos scheduling at existing events is separate (`detcore/src/lib.rs:844`). |

`--target-timeslice` is a cooperative deadline checked when control returns to
Detcore (`detcore/src/lib.rs:1387`). It cannot bound guest code that never
returns control. Chaos priority choices, schedule-event recording through
`--record-preemptions`, and syscall-boundary schedule replay are also distinct
from timer delivery. Their names alone are not grounds for a timer refusal.
Exact branch-based schedule replay still needs its own supported mechanism;
accepting a boundary-only option is not a claim that arbitrary recorded
preemption points can be reproduced.

DBT's translation quantum is separate backend work. It does not implement
Detcore's maximum-timeslice contract and is not changed by this inventory.
DBT's forced clear remains in place for its owner to remove: it is redundant
for admitted coordinated Hermit launches, which already carry a disabled
maximum, but remains reachable through standalone/default runtime input.
Direct Reverie e9patch admission is outside this Hermit change.

The manifest inventory contains no explicit positive maximum on a timerless
backend: all 503 explicit maximum tokens disable it. The 738 DBT/SaBRe cells
that inherit a default remain selected; this is a configuration inventory, not
a measurement of passing cells. No manifest cell is disabled by this change.

Two existing tests retain their bodies but are disabled with the named refusal:
`run_kvm_chaos_bounds_how_long_a_spinner_keeps_a_new_thread_waiting` supplies an
explicit positive maximum through its helper; and
`liteinst_replay_with_a_conflicting_epoch_is_refused_in_every_build` requests
preemption-point replay, which now refuses before epoch reconciliation. The
ptrace spinner and replay-epoch tests remain active. New admission tests check
the timer refusal separately from those unsupported functional expectations.
