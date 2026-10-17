# LiteInst fixture CPU and wall budgets

## Five tests at a time, October 17

Since c6c91e59 (2026-10-17) the node runs five stacks tests at once
(NEXTEST_TEST_THREADS=5). The five long tests then run side by side, and their
cgroup CPU grows with host contention: the same test measured 14.6 CPU seconds
in one full validation and 28.9 in another at the same head (033b1d55). In the
validation of c63f1d21 the inert-preload and disabled-altstack tests hit their
31- and 30-second caps (exit 124 from the CPU wrapper at 41.5 s and 37.9 s of
wall, far inside their 93- and 90-second wall backstops), so the binding limit
was CPU, not wall.

The retained receipts of the 15 full validations at width 5 (7a5b0f15 through
c63f1d21, 225 attempts, two CPU-censored) give the caps below, each
`ceil(1.5 * max)` over completions and censored failures alike. The wall
backstop stays three times the cap. The ten short tests' maxima (3.69 s at
most) keep the 22-second cap with more than 50% headroom, and the
constructor's 55 already exceeds 1.5 times its width-5 maximum.

| Test | Completed (width 5) | CPU range (seconds) | Censored | Old cap | CPU cap | Wall backstop |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `cold_constructor::genuine_detcore_constructor_uses_retained_disjoint_bootstrap_stack` | 15 | 13.770215–35.115861 | — | 55 | 55 | 165 |
| `genuine_detcore_reaches_fixed_guarded_callback_and_altstack` | 15 | 13.722306–33.721186 | — | 42 | 51 | 153 |
| `genuine_standalone_registers_fixed_guarded_altstack_without_continuation` | 15 | 11.131885–30.907684 | — | 36 | 47 | 141 |
| `inert_preload_does_not_reserve_or_register_runtime_stacks` | 14 | 10.881525–28.854773 | 32.216597 | 31 | 49 | 147 |
| `standalone_altstack_disabled_receives_no_owned_stack_credit` | 14 | 11.145251–27.992446 | 30.381779 | 30 | 46 | 138 |

The inert-preload and disabled-altstack caps come from their censored
observations (`ceil(1.5 * 32.216597) = 49`, `ceil(1.5 * 30.381779) = 46`): their
completion cost under that load is unknown, so these are conservative
allowances, not measured headroom over a completion. No assertion, selection,
retry or wrapper behaviour changes. The sections below are the earlier
measurements at one test at a time.

## Complete stacks node, October 16 evening

All 15 `hermit::liteinst_stacks` tests now have an independent CPU allowance
and a wall backstop of three times that allowance. The wall policy accommodates
waiting under host load; it is an authorized tolerance increase, not a claim
that a test needs three CPU seconds for each wall second. Termination grace
remains two seconds. Allocator tests, other binaries, required selections,
retries, and every fixture/source/placement assertion are unchanged.

The retained inventory has 274 finalized per-attempt cgroup CPU receipts:
272 successful completions and two CPU-censored failures. It covers 18 complete
15-test selections plus four separately diagnostic constructor replays.
These are mixed source and artifact cohorts, not 18 repetitions of this change.
The original `f141091f` wall timeout has no finalized individual CPU receipt
and is excluded from the CPU ranges.

| Test | Completed samples | CPU range (seconds) | CPU cap | Wall backstop |
| --- | ---: | ---: | ---: | ---: |
| `artifact::tests::credits_exact_local_relative_got_route_and_direct_tailcall` | 18 | 2.376328–3.546702 | 22 | 66 |
| `artifact::tests::inline_endpoints_require_owned_allocator_allocation_methods` | 18 | 0.001778–0.007302 | 22 | 66 |
| `artifact::tests::missing_build_finished_is_a_failed_current_artifact_selection` | 18 | 1.094687–2.182613 | 22 | 66 |
| `artifact::tests::observed_integrated_detcore_e8_tls_whole_recipes` | 18 | 0.006894–0.017616 | 22 | 66 |
| `artifact::tests::refuses_new_or_unrecognized_guest_symbol_versions` | 18 | 0.001485–0.008026 | 22 | 66 |
| `artifact::tests::unresolved_or_preemptible_indirect_calls_do_not_earn_a_path` | 18 | 0.035160–0.057074 | 22 | 66 |
| `cold_constructor::caller_stack_samples_never_receive_constructor_placement_credit` | 18 | 0.001490–0.010135 | 22 | 66 |
| `cold_constructor::genuine_detcore_constructor_uses_retained_disjoint_bootstrap_stack` | 22 | 18.243929–36.113714 | 55 | 165 |
| `cold_constructor::later_guard_cannot_alias_the_retained_bootstrap_extent` | 18 | 0.001459–0.006932 | 22 | 66 |
| `genuine_detcore_reaches_fixed_guarded_callback_and_altstack` | 17 | 17.998559–27.542858 | 42 | 126 |
| `genuine_standalone_registers_fixed_guarded_altstack_without_continuation` | 17 | 13.932757–21.467302 | 36 | 108 |
| `inert_preload_does_not_reserve_or_register_runtime_stacks` | 18 | 13.899600–20.004790 | 31 | 93 |
| `stack_observation::tests::rejects_foreign_and_unreached_callback_records` | 18 | 0.001491–0.001912 | 22 | 66 |
| `stack_observation::tests::rejects_unprobed_and_readable_guards` | 18 | 0.001398–0.001866 | 22 | 66 |
| `standalone_altstack_disabled_receives_no_owned_stack_credit` | 18 | 13.673469–19.622632 | 30 | 90 |

For the constructor, `ceil(1.5 * 36.113714) = 55`; for the callback test,
`ceil(1.5 * 27.542858) = 42`. The inert-preload and disabled-altstack maxima
require 31 and 30 seconds. All ten small controls retain the existing 22-second
CPU allowance, which exceeds their measured maxima by more than 50%.

The standalone-altstack completed maximum would require 33 seconds. Its newer
attempt at Hermit `6b9c57afbfc3cfb66155aba24e8e0c31fd5c290c` instead hit CPU22:
the kill triggered at 22.008933 seconds, and cleanup-inclusive CPU ended at
23.338877 seconds (30.296 wall). Its completion cost is unknown. The cap of
36 seconds is conservative after that censored failure; qualification must
also establish an uncensored current-source completion and at least 50%
measured CPU headroom. The timeout remains a failure, not a completion sample.
The same main run completed the constructor in 34.780724 CPU seconds and the
callback in 27.454128; therefore changing only their wall bounds was insufficient.

Exact prepared package, binary and test identities admit these allowances.
Every selected stack test gets its own first-priority timeout in the default
profile and profiles with local overrides. CPU and wall multipliers remain
independent and each is applied once. The effective wall must strictly exceed
that test's CPU cap plus cleanup. Original finite timeouts still constrain the
ordinary partition and allocator/procfs exceptions. The former comparison of
every test's maximum CPU against every other test's wall is intentionally
replaced for stack tests with an explicit wall override; it would incorrectly
reject CPU55/wall165 because an unrelated ordinary test keeps wall57. A finite
original default remains required, and missing or warning-only defaults refuse.

The exact tables and source/run identities are retained under measurement
inventory SHA256 `9871284b456fa1500339d3f7499f7292a906d5431ceb37c13ac030255588c84f`.
This table states measured budget evidence; passing the fresh canonical node
is a separate hand-back requirement. The earlier measurements and decisions
below remain historical evidence rather than the current stacks policy.

# Earlier October 16 fixture measurements (historical caps)

The full validation at `876ec26de98849ba789ba8612ebdfbb0d6134268` stopped
`genuine_detcore_reaches_fixed_guarded_callback_and_altstack` at 22.359546 CPU
seconds against a 22-second cap (27.983 seconds wall). This is a censored
failure: its completion cost under that load is unknown.

The table audits every test in `hermit::liteinst_stacks` (15) and
`hermit::liteinst_allocator` (9), using the cgroup CPU receipts from five full
validations. Each ran all 24 identities once, without retries or skips:
119 completed successfully and one hit its CPU cap. These are measurements at
different source/artifact cohorts, not repeated qualification of one head.

The five heads, newest first, are:

- `876ec26de98849ba789ba8612ebdfbb0d6134268`
- `cfc14f5310916644eafd2e21c94a890c019fc765`
- `41d13d7043f9e6608780673252ba654aefe03a36`
- `38b6ffd9c70a2d050b8b889a9b8d0be3f1cab066`
- `03f3d85dcc436d44bd51bcf957a4154675d23c61`

CPU includes the test's descendant processes. Values below use actual cgroup
usage, including cleanup; separate wait4 values are not substituted. A passing
fixture does not imply that its whole validation passed.

## Per-test evidence and caps

CPU ranges contain successful completions only. `A` means allocator and `S`
means stacks; the test names are exact. All values are seconds. The wall limit
remains 57 and termination grace remains 2 for every test. The ordinary CPU
limit remains 22. There is no new binary-wide exemption.

| Binary | Test | Completed CPU range across five attempts | Completed max wall | Old CPU cap | CPU cap |
| --- | --- | ---: | ---: | ---: | ---: |
| A | `artifact::tests::credits_exact_local_relative_got_route_and_direct_tailcall` | 2.250767–2.707840 | 2.856 | 22 | 22 |
| A | `artifact::tests::inline_endpoints_require_owned_allocator_allocation_methods` | 0.001765–0.004829 | 0.065 | 22 | 22 |
| A | `artifact::tests::missing_build_finished_is_a_failed_current_artifact_selection` | 1.179818–1.459527 | 1.640 | 22 | 22 |
| A | `artifact::tests::observed_integrated_detcore_e8_tls_whole_recipes` | 0.007128–0.010894 | 0.044 | 22 | 22 |
| A | `artifact::tests::refuses_new_or_unrecognized_guest_symbol_versions` | 0.001974–0.004821 | 0.038 | 22 | 22 |
| A | `artifact::tests::unresolved_or_preemptible_indirect_calls_do_not_earn_a_path` | 0.034053–0.037996 | 0.046 | 22 | 22 |
| A | `genuine_detcore_leaf_keeps_scope_free_work_private` | 19.446122–20.754169 | 26.942 | 31 | 32 |
| A | `genuine_standalone_leaf_keeps_scope_free_work_private` | 14.124147–14.513757 | 21.057 | 33 | 33 |
| A | `source_copy::gitless_standalone_compiler_sources` | 2.843885–3.071309 | 3.599 | 22 | 22 |
| S | `artifact::tests::credits_exact_local_relative_got_route_and_direct_tailcall` | 2.399077–2.803949 | 2.935 | 22 | 22 |
| S | `artifact::tests::inline_endpoints_require_owned_allocator_allocation_methods` | 0.001839–0.003097 | 0.039 | 22 | 22 |
| S | `artifact::tests::missing_build_finished_is_a_failed_current_artifact_selection` | 1.287499–1.853200 | 2.044 | 22 | 22 |
| S | `artifact::tests::observed_integrated_detcore_e8_tls_whole_recipes` | 0.006894–0.013295 | 0.046 | 22 | 22 |
| S | `artifact::tests::refuses_new_or_unrecognized_guest_symbol_versions` | 0.001662–0.003331 | 0.082 | 22 | 22 |
| S | `artifact::tests::unresolved_or_preemptible_indirect_calls_do_not_earn_a_path` | 0.035496–0.049163 | 0.056 | 22 | 22 |
| S | `cold_constructor::caller_stack_samples_never_receive_constructor_placement_credit` | 0.001788–0.002962 | 0.015 | 22 | 22 |
| S | `cold_constructor::genuine_detcore_constructor_uses_retained_disjoint_bootstrap_stack` | 21.742018–26.386976 | 34.903 | 33 | 40 |
| S | `cold_constructor::later_guard_cannot_alias_the_retained_bootstrap_extent` | 0.001459–0.005124 | 0.041 | 22 | 22 |
| S | `genuine_detcore_reaches_fixed_guarded_callback_and_altstack` | 17.998559–21.060359 | 26.772 | 22 | 34 |
| S | `genuine_standalone_registers_fixed_guarded_altstack_without_continuation` | 13.932757–14.452312 | 19.574 | 22 | 22 |
| S | `inert_preload_does_not_reserve_or_register_runtime_stacks` | 13.899600–14.308223 | 20.372 | 22 | 22 |
| S | `stack_observation::tests::rejects_foreign_and_unreached_callback_records` | 0.001542–0.001754 | 0.066 | 22 | 22 |
| S | `stack_observation::tests::rejects_unprobed_and_readable_guards` | 0.001514–0.001745 | 0.012 | 22 | 22 |
| S | `standalone_altstack_disabled_receives_no_owned_stack_credit` | 13.673469–14.474550 | 20.045 | 22 | 22 |

The Detcore allocator cap is `ceil(1.5 * 20.754169) = 32`; the constructor cap
is `ceil(1.5 * 26.386976) = 40`. The other ordinary tests' completed maxima
still fit their existing 22-second cap with this margin.

The callback test's completed canonical maximum, 21.060359, would yield 32.
Its interrupted 22.359546 observation is larger. The chosen 34 is the
conservative `ceil(1.5 * 22.359546)` allowance, not a claim of 50% headroom over
an observed completion under that load. Three serial replays of its unchanged,
retained 876ec26d source and prepared binaries completed with a diagnostic
33-second CPU cap and the original 57-second wall:

| Replay | CPU | Wall |
| --- | ---: | ---: |
| 1 | 19.476304 | 25.001 |
| 2 | 20.099408 | 26.750 |
| 3 | 19.744570 | 24.829 |

Each replay selected one test, ran one attempt, and preserved its source and
artifact hashes. They are uncensored diagnostics, not successes under the old
22-second cap or runtime qualification of a newer source/pin. An initial replay
launcher assertion failed before any test ran; that failed preparation is
retained separately from these three executed samples.

The standalone allocator keeps its previously accepted 33-second allowance.
Its older d7efd4cb run hit CPU22 at 22.799306, while three retained diagnostics
completed at 17.718674, 17.632669 and 17.573148. The lower recent maximum does
not erase that pressure evidence or justify reducing the existing cap.

The allowance resolver still requires the exact package, binary and test name
in a prepared inventory. Unit checks cover neighboring identities, independent
scaling and CPU/wall inversion. Semantic assertions, required test counts,
retries, source/artifact qualification and the unrelated procfs test's
36-second allowance are unchanged. No full validation or manifest parity
claim follows from these timing measurements.

Evidence summary SHA256: `23ec442b9f3dfa3e85fe0a3d83c24ff203e536ca0c7db948aecaea2952d3a41b`.
Canonical 120-attempt table SHA256: `59805a2dd0db2fdca7e22e2a1357d07e6a315d8bc40b907cac55337144ce1489`.
The original receipts, copied runtime closure and failed attempts remain in the
lane's handback evidence; the hashes identify those records without embedding
machine-local paths or raw logs here.
