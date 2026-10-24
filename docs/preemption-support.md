# Preemption support

`--max-timeslice` promises to regain control from guest code even if it makes
no system call. A logical clock, guest POSIX timer, or scheduling decision at a
system call does not supply that mechanism.

Hermit's CLI preserves an omitted maximum and the existing scheduling behavior.
The parser default remains 200,000,000 virtual nanoseconds; the absence of a
timer mechanism does not rewrite it to `None`. An explicit positive maximum, including
`--max-timeslice=200000000`, is refused before guest startup on DBT, SaBRe, KVM,
LiteInst and in-guest-trap. Config-file and `--from` values are explicit inputs
too; they retain the same precedence as command-line values. Explicit disabled
or zero maximums pass this timer admission check. Ptrace and e9patch
preprocessing keep their existing timer behavior and host-counter fallback.
LiteInst and in-guest-trap retain their earlier refusal of any configured
positive maximum, including the inherited default; their default is not newly
accepted.

The CLI captures the effective option's source after config/`--from` expansion,
independently of saved-config capture. An explicit value equal to the default
is still explicit. `--preemption-timeout` is the maximum's alias; disabling
RCB logical time with `--no-rcb-time` does not cancel an explicitly requested
maximum. This admission policy does not introduce a scheduler policy or a
timer implementation.

The refusal is `unsupported_preemption: --backend=NAME refuses FLAG: this
runtime cannot deliver Detcore preemption timer events; the guest has not
started`, classified as `HERMIT_POLICY_REFUSAL` (exit 122). Timer-only requests
are refused even when the maximum is disabled, including
per-thread preemption-point replay. Chaos controls, including per-thread RCB
slowdown, and schedule-event recording/replay retain their existing behavior.

Configuration and timer availability are separate metadata. The optional
`preemption` object in a summary or run manifest contains `max_timeslice_ns`
(null means the configured maximum is disabled) and `timer_enabled` (false
means no supported timer, true means enabled, and null/missing means unknown
in older metadata). Human output says `Configured maximum timeslice: N virtual
ns` or `disabled`, followed by `timer: none` or `timer: enabled` when known. A
positive inherited maximum can therefore coexist with `timer: none`. These
fields describe configuration and capability, not an observation that a timer
was armed or delivered. An absent `preemption` object in an older summary means
unknown. The full human summary includes this metadata; the existing INFO
guest-behavior comparison is unchanged.

Run options and verbose diagnostics retain the configured value. An omitted
maximum stays omitted in `--save-config`, so reloading uses the same parser
default. Runtime metadata belongs in the manifest's optional top-level
`preemption` object, not in its replayable command-line configuration.

DBT's existing CLI summary reports native counters separately; it prints the
same supplied-configuration metadata. This change does not add a DBT CLI
`--summary-json` producer where none existed.

Direct library launch APIs receive `DetConfig` without CLI provenance and keep
their baseline behavior. The new CLI admission does not add a global library
refusal or normalize library defaults. The existing in-guest LiteInst/trap
library guard still refuses a positive maximum. DBT's backend-owned
configuration clear also remains in place.

The shared behavior below is grounded in Hermit baseline
`4d7023ce0d201d45280903425a847432c2377ad3` and its Reverie pin
`c4fb4d70aa6d5d61545a4759289f101529329afd`. The timer methods also have the
same behavior at the earlier Reverie pin
`75575dc018555cc08531b2b2204045a6fc9cfbbb`. This is implementation evidence,
not measured parity or a claim of bounded execution on a timerless backend.

| Execution path | What happens to a requested maximum | Source |
| --- | --- | --- |
| ptrace | Arms a retired-conditional-branch timer. The existing CLI warning/fallback disables the maximum when the host counter/profile is unavailable. | Reverie `reverie-ptrace/src/task.rs`, `set_timer`; Hermit `RunOpts::validate_args` |
| DBT | `assert_dbt_execution_model` forcibly sets the maximum to `None` before Detcore runs. Its sampled syscall-entry branch count does not provide a threshold timer; timer methods return `ENOSYS`. | Hermit `detcore-dbt/src/lib.rs:460`; Reverie `reverie-dbt/src/lib.rs:751` |
| LiteInst and in-guest-trap | The earlier CLI/library guards refuse any positive configured maximum, including the inherited default. The shared in-guest timer methods return `ENOSYS`; clock reads remain available. | Hermit `refuse_in_guest_liteinst_run` and the CLI in-guest guard; Reverie `reverie-inguest/src/guest/host.rs:3348` |
| SaBRe | Preserves the inherited maximum. `read_clock` returns zero; timer methods report success without arming a timer. The positive maximum still affects shared logical-time costs and deadline selection. | Reverie `experimental/reverie-sabre/src/reverie_adapter.rs:1482` |
| KVM | Preserves the inherited maximum and samples its actual guest RCB counter. Its count-only clock does not deliver timer events; timer methods return success without arming one. | Reverie `reverie-kvm/src/clock.rs`, `read_clock`; `reverie-kvm/src/runtime.rs:1789`; `reverie/src/capabilities.rs:323` |
| Hermit e9patch preprocessing | Executes through ptrace after rewriting and uses ptrace's timer. | Hermit e9patch preprocessing and ptrace dispatch in `hermit-cli/src/bin/hermit/run.rs` and `hermit-cli/src/lib.rs` |
| Direct Reverie e9patch Tool host | Timer methods return an Unsupported error. This is a separate execution path and outside this CLI admission change. | Reverie `reverie-e9patch/src/tool_host.rs`, `set_timer` |

The parser default is defined in `detcore-model/src/config.rs`.
`--max-timeslice=disabled` and
`--max-timeslice=0` explicitly request no maximum. `--preemption-timeout` is an
alias. `--no-rcb-time` changes the logical clock; it does **not** disable an
active maximum.

## Shared uses of the configured maximum

`max_timeslice` is not only a timer-arm switch. Preserving its inherited value
preserves these shared behaviors:

* `Detcore::update_logical_time_rcbs` samples `Guest::read_clock` whenever the
  maximum is present, updates the committed branch count, and uses the delta
  for RCB time and recorded branch events when enabled (`detcore/src/lib.rs`).
* `Config::use_rcb_time` is exactly `max_timeslice.is_some() && !no_rcb_time`.
  `DetTime::new` applies a 500x multiplier to modeled event costs when threads
  are sequentialized and RCB time is off, in addition to any configured clock
  multiplier (`detcore-model/src/config.rs`, `detcore-model/src/time.rs`).
  Clearing the maximum changes those syscall/instruction costs even on SaBRe,
  whose branch-clock reads return zero.
* `ThreadState::next_timeslice` starts with
  `target_timeslice.or(max_timeslice)`. Ordinary execution, chaos sampling and
  replay then choose the logical slice end and maximum deadline. Both absent
  clears those deadlines (`detcore/src/tool_local.rs`). Changing the maximum
  therefore changes cooperative scheduling and deadline progress as well as
  timer requests.
* `pre_handler_hook` samples time and checks the logical deadline at an existing
  guest boundary. `post_handler_hook` attempts `set_timer` or
  `set_timer_precise` when `max_timeslice_end` exists; the dropped-signal path
  also re-arms an existing timer when the maximum is present. Keeping these
  baseline calls does not make KVM's or SaBRe's no-op timer methods deliver an
  event (`detcore/src/lib.rs`).

KVM's guest-only performance counter supplies real branch deltas between guest
entries. Removing the maximum would suppress those samples and select the
500x no-RCB cost path. SaBRe has no exposed branch progress, but removing its
maximum would still change the cost multiplier and logical deadlines. These
backend differences are why timer capability alone cannot normalize the shared
configuration without changing scheduling behavior.

## Other settings

| Setting | Relevant behavior |
| --- | --- |
| `--imprecise-timers` | Selects an imprecise rather than precise timer arm in `post_handler_hook`. |
| `--interrupt-at` | Needs a timer and RCB logical time to stop at the requested branch count. |
| `--preemption-stacktrace`, `--preemption-stacktrace-log-file` | Produce the requested output in the timer-event handler. |
| `--panic-on-rbc-overshoot` (alias `--panic-on-rcb-overshoot`) | Checks a precise timer's branch-count overshoot during RCB sampling. |
| `--skid-margin` | Configures the ptrace PMU timer, not a guest clock. |
| `--replay-preemptions-from` | Loads per-thread preemption points. This CLI request refuses on a timerless backend rather than reducing the recording to a cooperative subset. |
| `--chaos-per-thread-slowdown` | Retains its existing acceptance. It weights sampled RCB time at callbacks and does not itself need timer delivery. Its effect depends on the backend's clock progress and `use_rcb_time`. |

`--target-timeslice` is a cooperative deadline checked when control returns to
Detcore. It cannot bound guest code that never
returns control. Chaos priority choices, schedule-event recording through
`--record-preemptions`, and syscall-boundary schedule replay are also distinct
from timer delivery. Their names alone are not grounds for a timer refusal.
Exact branch-based schedule replay still needs its own supported mechanism;
accepting a boundary-only option is not a claim that arbitrary recorded
preemption points can be reproduced.

Shared replay behavior is unchanged: per-thread preemption histories take
precedence over ordinary chaos deadlines; schedule replay can replace the
logical slice end, and `trace_schedevent` updates the maximum deadline when
a maximum is configured. Chaos may sample a shorter slice and enables
sequentialized threads. `--record-preemptions-to` implies event recording.
These existing interactions still use the configured maximum; preserving the
default does not certify precise replay on a backend without timer delivery.
Per-thread chaos slowdown weights RCB deltas at callbacks, so a working branch
clock and timer delivery are separate requirements. KVM can supply those deltas
without a preemption timer. SaBRe's zero clock supplies no branch progress;
DBT's forced clear disables shared RCB-time accumulation; and the existing
in-guest maximum refusal leaves shared RCB time off when callers disable the
maximum. These clock differences are documented without adding a new chaos
refusal or changing the existing weighting policy.

DBT's translation quantum is separate backend work. It does not implement
Detcore's maximum-timeslice contract and is not changed by this inventory.
DBT's forced clear remains necessary to its existing execution model: the CLI
now preserves the inherited positive maximum, and the DBT timer methods would
otherwise fail with `ENOSYS`. The clear also determines DBT's shared no-RCB
clock/cost behavior. It is not redundant and is not removed here.
Direct Reverie e9patch admission is outside this Hermit change.

The earlier manifest inventory at Hermit
`532527297e278c30095427ea61e7fd23c5065b2f` found no explicit positive maximum
on a timerless backend: all 503 explicit maximum tokens disable it. The 738
DBT/SaBRe cells that inherited a default remain selected with baseline backend
behavior. These are configuration counts, not measurements of passing cells.
This change adds no default-based cell disables.

Two existing tests retain their bodies but are disabled with the named refusal:
`run_kvm_chaos_bounds_how_long_a_spinner_keeps_a_new_thread_waiting` supplies an
explicit positive maximum through its helper; and
`liteinst_replay_with_a_conflicting_epoch_is_refused_in_every_build` requests
preemption-point replay, which now refuses before epoch reconciliation. The
ptrace spinner and replay-epoch tests remain active. New admission tests check
the timer refusal separately from those unsupported functional expectations.

The ten existing KVM clock/deadline regression expectations remain unchanged;
the default-preservation checks must keep their guest assertions. The SaBRe
regression `sabre_hands_a_readied_pipe_poller_the_turn_across_exec` also belongs
in validation: it checks the default writer/reader ordering, zero-latency handoff,
and unchanged turns for actions that ready no poller. Admission tests alone do
not validate these scheduling behaviors. This document does not claim those
gates have passed.
