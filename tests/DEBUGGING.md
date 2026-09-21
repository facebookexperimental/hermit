# applications

# backend-parity-c

## backend-parity-c/hardware-trap-identity

### 2026-08-27T05:27:07-07:00 — portable / verify / dbt — ERROR

- Hermit: `6172478eae288c9f5005545d4f68c00c780f41c0`; complete published E2E
  artifact binary SHA-256:
  `82e5d9e0f1ba554a2cedf4e970e94c43761bbca42087a2797443aca22c0da695`.
  Later main commit `f857a94ef7f943cacb2827177d876a296cfde989`
  moved the Reverie pin onto main without changing the DBT runtime source.
- Command: each retained result row contains the runner-generated `hermit --log
  info run --base-env=minimal --backend dbt --strict --verify-strict --verify
  --keep-logs` invocation. The checked-in timeout remained 15 seconds, one
  scheduled worker, and no comparison relaxation.
- Evidence: three result rows and their artifacts are retained under
  `<dev-hermit>/ignored/hermit-132-postfix-6172478e-retry4-results/`. The two
  complete Run1 logs have SHA-256
  `03a22d52750074e8d661e173540a1495f288272c3d4fa67b68e57a78adbc51bb`.
  The pre-fix record contents and artifact hashes remain preserved in
  [`tests/backend-parity/README.md`](backend-parity/README.md#hardware-trap-identity-pre-fix-dbt-child-exit-polling-changed-the-canonical-log).
- Observed: 3 of 3 attempts returned `ERROR` with `verdict=no_result`; no
  terminal comparison exists. Two attempts completed an identical Run1 trace
  with no `InternalIOPolling` and ended after the root entered `exit_group(0)`,
  then timed out after starting Run2. The other attempt timed out in Run1 with
  an empty log.
- Current explanation: before Hermit `221c3d77959ebdef08f2890aaf3ce5185ea5d425`,
  DBT alone set `backend_tracks_process_children` false. When the parent reached
  blocking `wait4` before the child exit was published, Detcore used the generic
  nonblocking retry path and recorded a host-dependent number of
  scheduler-visible polling turns. The landed repair removes that record
  pattern; it does not complete the exact cell. The guest's #UD, #DE, and #PF
  signals and `si_code` values were correct in the pre-fix direct runs, so the
  cell name does not identify the failing subsystem.
- Ruled out / next: this is not the SaBRe TLS ordering defect and not KVM's
  wrong guest-visible signal result. The pre-fix polling mechanism was shared
  with `backend-parity-c/signal-waitstatus-identity/verify@dbt`, which is also
  now 3 of 3 `ERROR/no_result`. Determine why DBT does not complete both runs
  after the child-lifecycle repair; do not restore host polling and do not call
  either cell resolved until a terminal canonical comparison exists.

## backend-parity-c/signal-waitstatus-identity

### 2026-08-27T05:27:07-07:00 — portable / verify / dbt — ERROR

- Hermit and command: the same main SHA, complete E2E artifact, unrelaxed
  comparator, 15-second timeout, and one-worker runner as
  `backend-parity-c/hardware-trap-identity` above.
- Evidence: three result rows and artifact directories under
  `<dev-hermit>/ignored/hermit-132-postfix-6172478e-retry4-results/`, with
  result-row SHA-256 values
  `585956b5b9a61009b0abac79befa75db622c1bc3df83c6937d78db27ddbefbfd`,
  `abe3254fd3b79e28318463892b7d57c963d453e729386d5ca06287945d952d19`,
  and `ef2bdddcf6039c044c0a43dcacde54d53728dc46d908b4af7b7ffe51edb83e48`.
- Observed: 3 of 3 attempts returned `ERROR` with `verdict=no_result`; each
  stopped in DBT Run1 and retained an empty canonical log. No coordinate or
  record contents exist to import.
- Current explanation: the old 3-of-3 divergence shared hardware-trap's
  `wait4`/`InternalIOPolling` mechanism while guest output ended in
  `failures=0`. The landed child-lifecycle repair removed that host-polling
  path, but this exact signal-terminated-child scenario still does not complete.
- Ruled out / next: ptrace was the positive control for the old comparison at
  336/336 canonical INFO records. Re-measure DBT only after its run completes;
  until then this cell is uncheckable rather than matching or diverging.

# bin-c

# c-programs

## c-programs/writev-determinism

### 2026-09-02 — historical CPU variation followed changing test work

- Scope: the reported solo cohort contained 27 `PASS` observations from full
  validates on one recorded host, at outer DAG width 16 and inner manifest
  width 8.
  Its unpartitioned CPU result was mean 0.545198 seconds, sample standard
  deviation 0.480885 seconds, and CV 0.882. Every observation had exactly one
  attempt, so retries did not contribute.
- Distribution: partitioning those same observations by the result row's
  `test_sha256` changes the result materially:

  | `test_sha256` prefix | n | CPU mean | sample SD | CV | scheduler turns | syscalls |
  |---|---:|---:|---:|---:|---:|---:|
  | `3c0b82fdee9b` | 18 | 0.247418s | 0.022896s | 0.093 | 204 | 147 |
  | `45d3c45959c4` | 2 | 0.483777s | 0.095295s | 0.197 | 613 | 299 |
  | `8c60c1f11b33` | 5 | 1.313803s | 0.151321s | 0.115 | 4062 | 308 |
  | `3cdfef2f2edd` | 2 | 1.365126s | 0.030647s | 0.022 | 4474 | 458 |

  Differences between those four source-group means account for 98.16% of
  the cohort's total squared CPU deviation; 1.84% remains within the groups.
  Within each hash, the observation SHA-256 and stdout were identical. The
  first group emitted only `writev-stdout` and `writev-determinism-ok`; the
  later groups added the two-blocked-writer check, six signal/partial-write
  checks, or both. Commit `d04efc883640ff111781c6f324e3ed10c60f52ad`
  adds the signal/partial-write cases to the default path, and
  `c4bdf4d4d366931f9f1778a6f7325aaffd3dd95d` adds the two-blocked-writer
  check. Their larger stable scheduler-turn and syscall counts are direct
  evidence that the later fixture versions intentionally execute more work.
- Build and accounting: full validate invokes this bucket with `--prebuilt`.
  On that path `prepare_test_until` copies the prepared C fixture and does not
  invoke `cc`; the row's CPU value equalled its sole attempt's CPU value in all
  27 observations. The measurement is the specific launched child's
  `wait4(2)` user plus system CPU, including descendants it reaped; it is not
  the enclosing DAG cgroup counter. Cached compilation, cgroup contamination,
  and retries therefore do not explain this distribution.
- Interpretation: the cross-version CV compares different work and is not
  evidence that one version performs a non-conserved amount of work. The
  corresponding within-version CPU CVs are 0.022–0.197; the largest value has
  only two observations. The other nine cells in the original CPU top ten each
  had one `test_sha256`, so source-version mixing did not explain their ranks.
  As a same-SHA check outside the ranked solo cohort, two observations at
  `b779f99f2add10d9fa198466efd2dbce0edbf689` with the same Hermit binary and
  test hash used 0.210147 and 0.215621 CPU seconds.
- Debugging rule: before interpreting cell runtime variance, partition rows by
  `test_sha256` and `binary_sha256`, then hold host, outer/inner concurrency,
  and attempt count constant. Compare scheduler turns, syscalls, virtual time,
  observation hash, and stdout. A change in those work counters or outputs is
  a changed workload to explain before investigating contention or caches.

The raw fields can be inspected without rerunning the cell:

```bash
artifact_root=${DEV_HERMIT_ROOT:?set DEV_HERMIT_ROOT}/ignored/validate/artifacts
rg -l 'writev-determinism' \
  "$artifact_root" \
  -g results.jsonl |
while IFS= read -r result; do
  jq -r 'select(.test == "c-programs/writev-determinism" and
                .mode == "verify" and .backend == "ptrace" and
                .outcome == "PASS" and (.cpu_usage_usec | type) == "number") |
         [.run_id, .hermit_sha, .test_sha256, .binary_sha256,
          .cpu_usage_usec, .duration_ms, (.attempts | length),
          .runtime.run1.scheduler_turns, .runtime.run1.syscalls,
          .attempts[0].observation_sha256] | @tsv' "$result"
done
```

The producer implementation is
`ci/manifest-plan/src/runner.rs`: `prepare_test_until` chooses prebuilt-copy
versus compilation, `rusage_cpu_usage_usec` reads `wait4` user and system CPU,
`run_cell` sums preparation and attempt CPU, and `test_digest` hashes the guest
source bytes.

# chaos-c

# data-handling

## data-handling/dd-partial-transfers

### 2026-09-02 — portable / verify / ptrace — fixed work stopped by the wall deadline

- Classification: this is a fixed-work cell with an undersized wall bound, not
  a cell that conditionally builds or selects a variable workload. Its
  `--prepare` action is a no-op. Each guest execution generates exactly 4096
  bytes, copies 4096 bytes through `dd bs=1` over a pipe, copies the same 4096
  bytes through file-backed `dd bs=1`, and transfers one final 89-byte input in
  13 seven-byte blocks. A healthy strict verification performs that guest work
  twice.
- Controlled command: the following is the direct form used for two independent
  measurements. It requires a built Hermit and the parent repository's
  `bin/safehermit`; replace the two path variables for another checkout. The
  measured binary reported `hermit 0.2.0 (2026-09-02, g5910bc208d13)`, had
  SHA-256 `68b548ee25b7f6b16244ec5e7a9a4d9cf300936c4c38ee9c8225a7cc8582251f`,
  and ran a guest script with SHA-256
  `30b56fd75af20124a68e28d744e54bd82b0161e5b91304a3fb047a23d66fd6b2`.
  The backend was ptrace, the log level was info, and there were no
  determinism relaxations.

  ```bash
  dev_hermit=${DEV_HERMIT_ROOT:?set DEV_HERMIT_ROOT}
  hermit_bin=${HERMIT_BIN:?set HERMIT_BIN to the measured Hermit binary}
  run=$(mktemp -d)
  mkdir -p "$run/home" "$run/xdg-config" "$run/fixtures"
  env E2E_FIXTURE_DIR="$run/fixtures" E2E_TMPDIR=/tmp/hermit-e2e \
    HERMIT_E2E_SCHEDULED_JOBS=1 HOME="$run/home" LC_ALL=C TZ=UTC \
    XDG_CONFIG_HOME="$run/xdg-config" \
    "$dev_hermit/bin/safehermit" --sh-deadline 120 \
    --sh-report "$run/safehermit.report" \
    "$hermit_bin" --log info run --base-env=minimal --backend ptrace \
    --strict --verify-strict --verify --verify-json "$run/verify.json" \
    --mount=type=tmpfs,target=/test --workdir /test --env LC_ALL=C \
    --env TZ=UTC --env HOME="$run/home" \
    --env XDG_CONFIG_HOME="$run/xdg-config" --env E2E_TMPDIR=/test \
    --env E2E_FIXTURE_DIR="$run/fixtures" \
    --env HERMIT_E2E_SCHEDULED_JOBS=1 -- \
    "$PWD/tests/e2e/data-handling/dd-partial-transfers.sh" --run
  jq '{verdict, bitwise_parity, compared_log_messages, runtime}' \
    "$run/verify.json"
  ```

- Real output: two fresh-home invocations on a 316-CPU host each completed in
  10 seconds and produced byte-identical reports. Both internal executions in
  both invocations recorded exactly 18,616 syscalls, 8,832 scheduler turns,
  4,777,481,955 virtual nanoseconds, and 63,224 canonical INFO messages:

  ```json
  {"verdict":"matched","bitwise_parity":true,"compared_log_messages":{"left":63224,"right":63224},"runtime":{"run1":{"scheduler_turns":8832,"virtual_nanoseconds":4777481955,"syscalls":18616},"run2":{"scheduler_turns":8832,"virtual_nanoseconds":4777481955,"syscalls":18616}}}
  ```

- Independent control: three fresh-home invocations of exact Hermit
  `a6b0c37648df774d5859aad23a535caf03a6d392` on
  `devbig030.atn3.facebook.com` each completed in 9 seconds with matched L2
  reports and exactly 63,224 INFO messages on both sides. Other retained SHAs
  record a different but internally equal count; compare work counters only
  within one exact binary and source revision.
- Failure evidence: hosted CI at `6fab069e01096d174a196f9a658b7d5468d494d5`
  completed Run1 twice, entered Run2, and then received SIGTERM after 14.657
  and 14.664 seconds. A concurrent local run completed one attempt in 14.019
  seconds, while its peer was killed at 15.042 and 15.036 seconds after using
  14.298 and 6.466 process CPU-seconds. The smaller CPU total belongs to an
  incomplete execution; it is not evidence that the cell selected less work.
- Follow-up checked on 2026-09-12: the manifest now configures a 58-second wall
  bound and a 22-second CPU bound. Its 60 retained typed passing rows had a
  nearest-rank p90 of 14.275 wall seconds and 12.732855 CPU seconds; the
  calibration requires `ceil(4 * wall) = 58` and `ceil(1.5 * CPU) = 20`,
  covered by the configured 22-second CPU default. The historical 15-second
  failures above do not describe the current bound. The guest source hash and
  full transfer population remain unchanged. See
  `tests/e2e/manifests/data-handling.yaml` and
  `ci/manifest-plan/src/timeouts.rs` for the measured policy. Preserve that
  fixed workload when investigating future runtime variation.

# debugger-c

# determinism-stress

# determinism-stress-c

# language-runtimes

# shared-futex-c

# system-utils

# util-c

# Validate test-ID debugging guide — 2026-08-27

This guide preserves the evidence behind the test IDs observed red in the
retry-aware validation at Hermit
`a6b0c37648df774d5859aad23a535caf03a6d392`. It is a debugging handoff, not a
failure-rate estimate and not a suppression list.

## Measurement boundaries

The `a6b0c37648df` run was on `devbig014.atn7.facebook.com`, kernel
`6.13.2-0_fbk17_hardened_0_g2ae417e0caa0`, from
`2026-08-27T02:37:48Z` through `03:08:02Z`, at DAG width 16. One other full
validate was active during part of it. The retained evidence contains 677
individual nextest IDs and 1,350 attempts: 21 IDs failed every recorded attempt,
7 had both a pass and a non-pass, and 649 passed every recorded attempt. Those
are observations from at most two attempts per ID, not rates.

The attempt to replace that population with a current-main full validate ran at
`ff125248383e7d4ed04eb7e93b5fa8354d972483` on the same host and kernel. It is a
non-verdict: `pre.reverie_pin` failed both attempts because Hermit's pinned
Reverie commit `1393db6279331feb4af2533220e4786ab104a1b9` was not reachable
from then-current `reverie/main` `ab07a89239150df3726a036bee9f5e897893dfc1`.
Only 2 nodes executed; 64 were dependency-skipped and 2 were host-inapplicable.
No individual test ID ran. The durable full log is
`/home/newton/work/dev-hermit/ignored/validate/validate-full-ff125248383e-validate-hermit-134-debug-guide-ff125248383e.log`
and the ledger event is `devbig014-1787831073-3959920`.

A focused remeasurement of the 28 inherited IDs was therefore started at the
newer current-main SHA `6172478eae288c9f5005545d4f68c00c780f41c0` through
`validate-lock`. It uses `cargo nextest`, `--features third-party-backends`,
`-j 1`, two independent invocations, and the checked-in 15-second default
per-test cap plus named overrides. The validate runner owns the one-retry,
two-total-attempt cell policy; nextest's default profile does not. The focused
fallback therefore repeats the exact invocation instead of mislabeling one
nextest execution as two attempts. It can say which inherited IDs still fail;
it cannot discover a newly failing ID outside the inherited 28-ID population.

Primary retained evidence:

- prior full log:
  `/home/newton/work/dev-hermit/ignored/validate/validate-full-a6b0c37648df-validate-hermit-137-a6b0c37648df-1787798057339452591-387681-9a8a881f.log`
- prior artifacts:
  `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-137-a6b0c37648df-1787798057339452591-387681-9a8a881f/`
- prior ledger event: `devbig014-1787800082-538574`
- focused current-main attempt 1 stderr:
  `/home/newton/work/dev-hermit/ignored/validate/validate-hermit-134-focused-6172478eae28-5.stderr.log`
- focused current-main attempt 2 stderr:
  `/home/newton/work/dev-hermit/ignored/validate/validate-hermit-134-focused-6172478eae28-6.stderr.log`

## Focused observations at `6172478eae28`

The two independent invocations ran on `devbig014.atn7.facebook.com`, kernel
`6.13.2-0_fbk17_hardened_0_g2ae417e0caa0`, booted 2026-08-26 05:51:32, at the
same SHA and with the same filter. Nextest reported 201.542 seconds for the
first and 150.391 seconds for the second. `validate-lock` reported the other
slot anchor held when each invocation acquired its slot, so these are not
quiet-box timings. The first run record's free-text `measurement` field says
“two attempts each”; that label is wrong. Its stderr contains one nextest run.
The second run record and the two stderr logs are the evidence for the actual
two-invocation comparison.

The same 23 IDs were non-pass in both invocations and the same 5 passed in
both. That is an observation about this selected population, not a failure
rate. It also is not a replacement for a full validate: the filter cannot find
a newly failing ID outside these 28, and the full DAG prerequisites did not
run. In particular, the LiteInst cases reached an unrecorded staged runtime;
`ci/liteinst-strict-node.sh` defines that as a setup condition where nothing
about LiteInst was measured. Preserve those results as evidence about the
blocked path, not as product verdicts.

| Test ID | Invocation 1 | Invocation 2 |
|---|---:|---:|
| `hermit::app_strict_verify$go_goroutines_are_deterministic_under_strict_verify` | TIMEOUT 15.003s | FAIL 9.570s |
| `hermit::app_strict_verify$go_hello_is_deterministic_under_strict_verify` | TIMEOUT 15.003s | FAIL 13.272s |
| `hermit::app_strict_verify$python_is_deterministic_under_strict_verify` | PASS 1.264s | PASS 3.073s |
| `hermit::cli$every_record_container_site_classifies_a_child_fault_by_name` | TIMEOUT 30.002s | TIMEOUT 30.002s |
| `hermit::cli$run_dbt_fails_closed_by_default_and_opt_out_aggregates_unsupported_syscalls` | TIMEOUT 15.002s | TIMEOUT 15.002s |
| `hermit::cli$run_dbt_verifies_process_wait_lifecycle` | FAIL 1.619s | FAIL 5.584s |
| `hermit::cli$run_dbt_verifies_queued_self_signals` | FAIL 2.246s | FAIL 3.826s |
| `hermit::cli$run_dbt_verifies_self_prlimit` | FAIL 9.084s | FAIL 0.765s |
| `hermit::cli$run_dbt_verifies_shell_process_lifecycle` | FAIL 0.752s | FAIL 7.192s |
| `hermit::cli$run_dbt_verifies_simple_env_shebang` | FAIL 3.561s | FAIL 3.951s |
| `hermit::cli$run_dbt_virtualizes_process_identities` | FAIL 7.664s | FAIL 2.334s |
| `hermit::cli$run_liteinst_rejects_a_non_runtime_override_before_activation_claim` | FAIL 0.011s | FAIL 0.010s |
| `hermit::cli$run_liteinst_rejects_an_inert_dso_before_activation_claim` | FAIL 0.040s | FAIL 0.038s |
| `hermit::cli$run_liteinst_verifies_detcore_backend` | TIMEOUT 15.002s | FAIL 5.233s |
| `hermit::command_strict_verify$kernel_pseudofile_commands_are_deterministic_under_strict_verify` | FAIL 8.493s | FAIL 3.937s |
| `hermit::hermit_modes$verify_mode_matrix` | PASS 12.986s | PASS 12.762s |
| `hermit::hermit_modes$verify_reports_exit_status_divergence` | FAIL 1.620s | FAIL 0.820s |
| `hermit::hermit_modes$verify_reports_stdout_divergence` | FAIL 5.329s | FAIL 2.722s |
| `hermit::hermit_modes$verify_verbose_compares_the_full_trace` | FAIL 3.872s | FAIL 3.614s |
| `hermit::liteinst_advanced$liteinst_fork_fails_closed_without_hanging` | TIMEOUT 15.015s | FAIL 3.639s |
| `hermit::liteinst_advanced$liteinst_strict_verify_python_random_example` | TIMEOUT 15.003s | FAIL 1.678s |
| `hermit::liteinst_advanced$liteinst_strict_verify_round2_arithmetic_and_predicate_utilities` | FAIL 5.915s | FAIL 6.239s |
| `hermit::liteinst_advanced$liteinst_strict_verify_round3_stdin_filter_utilities` | FAIL 4.878s | FAIL 3.567s |
| `hermit::liteinst_advanced$liteinst_strict_verify_semantic_text_utilities` | FAIL 1.770s | FAIL 1.692s |
| `hermit::liteinst_advanced$liteinst_thread_clone_fails_closed_without_sigsys` | FAIL 6.539s | FAIL 6.342s |
| `hermit::sabre_examples$sabre_non_racy_examples_verify_current_envelope` | PASS 0.005s | PASS 0.004s |
| `hermit::signal_determinism$sigsuspend_without_signal_reports_terminal_deadlock` | PASS 3.847s | PASS 3.508s |
| `hermit::bin/hermit$run::detects_symlink_resolution_through_implicit_mounts` | PASS 0.007s | PASS 0.005s |

Relative to the earlier classification, 18 of the 21 IDs that had failed every
recorded attempt remained non-pass in both focused invocations; the symlink,
SaBRe-envelope, and `sigsuspend` IDs passed both. Five of the 7 IDs that had
both outcomes were non-pass in both; Python strict verification and
`verify_mode_matrix` passed both. That arithmetic produces the observed 23/5
split.

The `6172478eae28` failure text changes the debugging order for those focused
runs. Six DBT cases
(`process_wait_lifecycle`, queued self-signals, self-`prlimit`, shell process
lifecycle, the env shebang, and process identities) shared an RPC
`UnexpectedEnd` followed by “DBT evidence is missing FINAL frames for process
images.” The earlier per-test failures remain valid history, but this shared
failure happened first in that mixed-build evidence. The clean matched
remeasurement below is the authoritative current observation for the six DBT
tests. The current LiteInst failures likewise stop at the staged-runtime
revision check before the intended product behavior.

## Resolve the earlier shared failures first

Do not treat the six DBT entries above as six independent defects when
interpreting the two focused `6172478eae28` runs. All six stopped at the same
earlier transport/finalization failure. Diagnose that path once, using
[the retained DBT diagnosis](../ai_docs/2026-08-17-reverie-dbt-final-frame-diagnosis.md),
before using that mixed-build evidence to assess the older test-specific
failures.

That focused `6172478eae288c9f5005545d4f68c00c780f41c0` evidence is not
authoritative for the current state of these six tests. Those runs paired a
current binary with a packaged DBT client built from stale Reverie
`a16e3c466a15c3746a5ef23a76d1f74e11aba935`. A clean remeasurement on
2026-08-27 at the exact https://github.com/rrnewton/hermit/pull/2731 test head
`4944fb5b3cc029459056a3b9743f0d0df3ad0209` used a matched packaged client
from Reverie `ab07a89239150df3726a036bee9f5e897893dfc1`. It produced two
passes, the three earlier test-specific failures described below, and one
`simple_env_shebang` failure with a single process image missing `FINAL`; none
of the six results contained `UnexpectedEnd`. The retained node log is
`/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-141-4944fb5b3cc0-1787834650301238559-798721-c47ac7c9/dagrun/test.cli.log`,
and the full validate log is
`/home/newton/work/dev-hermit/ignored/validate/validate-hermit-141-4944fb5b3cc0-1787834650301238559-798721-c47ac7c9.log`.
Hermit source also changed between `6172478eae28` and `4944fb5b3cc0`, so this
is not same-SHA causal proof that the stale client alone caused the earlier
shared failure. Pull request 2731 only changes retained-log names and tightens
the test; it does not repair DBT execution.

Do not count the current LiteInst entries as measured product failures either.
They stop at a missing staged-runtime revision before reaching the behavior
their names describe. This is distinct from the missing `xxd` dependency that
blocks e9patch and SaBRe staging and is addressed by
https://github.com/rrnewton/hermit/pull/2744. The two setup failures affect
multiple backend measurements and review paths, but they have different causes
and must remain separate in debugging and reporting.

“Last green” below means the newest retained line for that exact nextest ID that
says `PASS`, joined to its run record. It does not turn a run-level non-verdict
into a green run. “Introduced” means the first commit containing the exact
current test ID; where a DBI-to-DBT rename created the current spelling, that is
stated explicitly.

## IDs that failed every recorded attempt at `a6b0c37648df`

### `hermit::app_strict_verify$go_goroutines_are_deterministic_under_strict_verify`

- Observation: `TIMEOUT 15.001s`, then `FAIL 10.032s` under one concurrent
  validate. The timeout was the configured cap; the failure says
  `hermit run --strict --verify was not deterministic (L2)`.
- Introduced: `6219d923ddc99db64af48afae8261cfe2ba2afc2`,
  2026-07-22, in `hermit-cli/tests/app_strict_verify.rs`.
- Last retained green: `PASS 9.984s` at Hermit
  `f77d7c44067a12ba11e75b5a85864ce0bc23e8f4`, run finished
  `2026-08-26T23:24:13.766596Z`; exact line in
  `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-105-f77d7c44067a-1787783997/dagrun/test.app_strict_verify.log`.
- Findings: one attempt hit the cap and one produced a real L2 mismatch. Do not
  combine those as one cause. The retained failure does not yet localize the
  first divergent record.

### `hermit::app_strict_verify$go_hello_is_deterministic_under_strict_verify`

- Observation: `FAIL 11.093s`, then `FAIL 10.161s`; both say
  `hermit run --strict --verify was not deterministic (L2)`.
- Introduced: `6219d923ddc99db64af48afae8261cfe2ba2afc2`,
  2026-07-22, in `hermit-cli/tests/app_strict_verify.rs`.
- Last retained green: `PASS 9.904s` at `f77d7c44067a12ba11e75b5a85864ce0bc23e8f4`,
  same run and log as the preceding ID.
- Findings: this is a repeatable strict-verification mismatch in the retained
  sample, not a timeout. No narrower cause was established.

### `hermit::bin/hermit$run::detects_symlink_resolution_through_implicit_mounts`

- Observation: `FAIL 0.007s` twice. Exact assertion:
  `path_resolution_visits_prefix(&proc_fd, Path::new("/tmp")).unwrap()`.
- Introduced: `720a059f0fb33dedf8025aebe5ceca8ecdf0f271`,
  2026-07-25, in `hermit-cli/src/bin/hermit/run.rs`.
- Last retained green before diagnosis: `PASS 0.004s` at
  `f77d7c44067a12ba11e75b5a85864ce0bc23e8f4`, in
  `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-105-f77d7c44067a-1787783997/dagrun/test.hermit_unit.log`.
- Findings: `NamedTempFile` follows `TMPDIR`, while validate redirects `TMPDIR`
  away from literal `/tmp`. This was a test environment assumption, not a
  product failure. Commit `e2d89097f1baa8af260c68994201121ac4c557da`
  now compares with `std::env::temp_dir()` and is on current main.

### `hermit::cli$every_record_container_site_classifies_a_child_fault_by_name`

- Observation: `TIMEOUT 30.002s`, then `TIMEOUT 30.003s`. This test has a
  measured 30-second override; both attempts reached that bound under a
  concurrent validate.
- Introduced: `1c738e76763a5f6016531debaadfa113f7ab2fc8`,
  2026-08-25, in `hermit-cli/tests/cli.rs`.
- Last retained green: `PASS 28.818s` at
  `f630aef3d18e87e49a1e099a5bcf4d2bf43987d1`, run finished
  `2026-08-27T10:51:24.977917Z`, in
  `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-130-f630aef3d18e-1787825672660163077-1370230-b619c59f/dagrun/test.cli.log`.
  That overall run was a non-verdict; the individual PASS line remains direct
  evidence.
- Findings: the green observation consumed 28.818 of its 30 seconds, so the
  test remains deadline-sensitive. No product failure text was emitted in the
  two capped attempts.

### `hermit::cli$run_dbt_fails_closed_by_default_and_opt_out_aggregates_unsupported_syscalls`

- Observation: `FAIL 3.739s`, then `FAIL 3.696s`. The opt-out `fork-exec`
  invocation exited 125 at the shared `cli.rs` command assertion.
- Introduced: `1f15510a6bb116c604a69d65975b3272a304ab7b`, commit date
  2026-08-24 (author date 2026-08-12), in `hermit-cli/tests/cli.rs`.
- Last retained green: `PASS 1.989s` at
  `8e2579d4f0640414f563a3f9f5e6eb4c21a0c884`, run finished
  `2026-08-26T20:55:44.967642Z`, in
  `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-001-8e2579d4f064-1787774099/dagrun/test.cli.log`.
- Findings: this is a DBT copied-process path. Earlier apparent DBT failures
  caused by building without `third-party-backends` were separately fixed by
  `740cc656`; the failing a6 run did build those features, so that older
  configuration defect does not explain this observation.

### `hermit::cli$run_dbt_verifies_queued_self_signals`

- Observation: `FAIL 0.491s`, then `FAIL 0.481s`. Exact inner text includes
  `rt_sigqueueinfo failed: result=-1 errno=3 delivered=1`; Hermit's first verify
  run exited 125.
- Introduced under the current exact ID by the DBI-to-DBT rename
  `e565b1ab1d5fcbbab492ffb886c893dbcb061ddf`, commit date 2026-08-08
  (author date 2026-08-06), in `hermit-cli/tests/cli.rs`.
- Last retained green: `PASS 0.455s` at
  `d19c112c7035672437b2a78d90a42fe7d690c6cb`, run finished
  `2026-08-25T11:26:31.756955Z`, in
  `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-014-d19c112c7035-1787655221/dagrun/test.cli.log`.
- Findings: this is an ESRCH/self-signal failure, distinct from the missing
  backend build configuration fixed by `740cc656`. The clean matched run at
  `4944fb5b3cc029459056a3b9743f0d0df3ad0209` reached the same historical
  `rt_sigqueueinfo` failure in 0.447s and emitted no `UnexpectedEnd`. Commit
  `6ba873cec2316f4f5d662487bf4d2b773795efdd` in draft
  https://github.com/rrnewton/reverie/pull/479 is a candidate change, not a
  proven fix.

### `hermit::cli$run_dbt_verifies_self_prlimit`

- Observation: `FAIL 0.484s`, then `FAIL 0.478s`. Exact inner text:
  `prlimit64 virtual-self mutation: Operation not permitted`; Hermit exited 125.
- Introduced under the current exact ID by `e565b1ab1d5fcbbab492ffb886c893dbcb061ddf`,
  same date and file as the preceding DBT ID.
- Last retained green: `PASS 0.428s` at `d19c112c7035672437b2a78d90a42fe7d690c6cb`,
  in the same retained `test.cli.log`.
- Findings: the guest queries with pid 0, then passes Hermit's virtualized pid
  back to `prlimit64`; that mutation returns `EPERM`. Prior triage places this
  at the DBT process-identity boundary, not in the missing-build configuration.
  The clean matched run at `4944fb5b3cc029459056a3b9743f0d0df3ad0209`
  reached the same historical `EPERM` failure in 0.471s and emitted no
  `UnexpectedEnd`; no draft pull request 479 fix is claimed for this case.

### `hermit::cli$run_dbt_verifies_shell_process_lifecycle`

- Observation: `FAIL 2.759s`, then `FAIL 2.700s`; the `/bin/sh` verify command
  exited 125 after protected records diverged around `wait4` and
  `InternalIOPolling`.
- Introduced under the current exact ID by `e565b1ab1d5fcbbab492ffb886c893dbcb061ddf`,
  in `hermit-cli/tests/cli.rs`.
- Last retained green: `PASS 0.745s` at
  `8e2579d4f0640414f563a3f9f5e6eb4c21a0c884`, in that run's retained
  `test.cli.log`.
- Findings: host-timed DBT child waiting changed the protected scheduler
  evidence. The shared repair landed through
  https://github.com/rrnewton/reverie/pull/506 and
  https://github.com/rrnewton/hermit/pull/2737; current Hermit main contains
  merge `20226fd5d6fc221da8e4f58341fed411248b1995`. The clean matched run at
  `4944fb5b3cc029459056a3b9743f0d0df3ad0209` reached a terminal comparison
  divergence on host `getpgrp` values in 2.678s and emitted no `UnexpectedEnd`.
  Commit `476f42770c2fa4472a3bb80798353503d83fdd2e` in draft
  https://github.com/rrnewton/reverie/pull/479 is a candidate change, not a
  proven fix.

### `hermit::cli$run_dbt_verifies_simple_env_shebang`

- Observation: `FAIL 1.231s`, then `FAIL 1.224s`. Exact inner error says the DBT
  guest exited 0 but protected evidence lacked `FINAL` frames for a process
  image, so the DBT run failed and Hermit exited 125.
- Introduced under the current exact ID by `e565b1ab1d5fcbbab492ffb886c893dbcb061ddf`,
  in `hermit-cli/tests/cli.rs`.
- Last retained green: `PASS 0.622s` at
  `8e2579d4f0640414f563a3f9f5e6eb4c21a0c884`, in that run's retained
  `test.cli.log`.
- Findings: this is evidence-finalization after a successful DBT guest, not a
  failure to launch `drrun` and not the earlier missing-feature defect. The
  clean matched run at `4944fb5b3cc029459056a3b9743f0d0df3ad0209` still
  lacked `FINAL` for one process image at epoch 0, next sequence 3, after
  1.074s, but emitted no `UnexpectedEnd`. Commit
  `9f5e0d9e2c0a51df8d460232ee75faf1a0e50974` in draft
  https://github.com/rrnewton/reverie/pull/479 is a candidate change, not a
  proven fix.

### `hermit::cli$run_dbt_virtualizes_process_identities`

- Observation: `FAIL 2.402s`, then `FAIL 2.323s`; the strict DBT verify command
  exited 125.
- Introduced under the current exact ID by `e565b1ab1d5fcbbab492ffb886c893dbcb061ddf`,
  in `hermit-cli/tests/cli.rs`.
- Last retained green: `PASS 2.572s` at Hermit
  `4944fb5b3cc029459056a3b9743f0d0df3ad0209`, in
  `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-141-4944fb5b3cc0-1787834650301238559-798721-c47ac7c9/dagrun/test.cli.log`.
- Findings: prior triage associates the failure with DBT process identity. The
  retained a6 failure does not isolate one identity syscall, so resume from the
  full stderr rather than assuming it is the same `prlimit64` failure. The
  clean matched run at `4944fb5b3cc029459056a3b9743f0d0df3ad0209` passed and
  emitted no `UnexpectedEnd`; because Hermit source and the packaged client
  both changed, that observation does not prove which change removed the
  earlier failure.

### `hermit::cli$run_liteinst_rejects_a_non_runtime_override_before_activation_claim`

- Observation: `FAIL 0.011s`, then `FAIL 0.009s`. Exact error: the `/bin/true`
  override `records no Reverie revision`, so it cannot be shown to match the
  Hermit pin.
- Introduced: `7f893d097270a3029c074b970a18ef9311e54958`,
  2026-07-30, in `hermit-cli/tests/cli.rs`.
- Last retained green: `PASS 0.012s` at
  `992b7eb5b7922d55233ceda5a29afedcace03242`, run finished
  `2026-08-25T15:21:08.052800Z`, in
  `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-005-992b7eb5b792-1787669771/dagrun/test.cli.log`.
- Findings: the expected-refusal test was itself rejected earlier than the
  assertion it meant to exercise. The dedicated staging fix was developed as
  `8f7b74095e57aaea73bd5e0539b92622a6a6003a`, but that commit is not an
  ancestor of current main as of `6172478eae28`.

### `hermit::cli$run_liteinst_rejects_an_inert_dso_before_activation_claim`

- Observation: `FAIL 0.042s`, then `FAIL 0.037s`. Exact error: the staged inert
  DSO `records no Reverie revision`, so it cannot be shown to match the pin.
- Introduced: `7f893d097270a3029c074b970a18ef9311e54958`,
  2026-07-30, in `hermit-cli/tests/cli.rs`.
- Last retained green: `PASS 0.044s` at `992b7eb5b7922d55233ceda5a29afedcace03242`,
  in the same retained `test.cli.log`.
- Findings: same staging-marker precondition as the preceding test, not a
  LiteInst guest-behavior failure. The unlanded staging commit named above must
  not be described as present on current main.

### `hermit::cli$run_liteinst_verifies_detcore_backend`

- Observation: `TIMEOUT 15.002s` twice; no product assertion was reached.
- Introduced: `138922c767e2415851dea1201956cfd43b7867eb`,
  2026-07-26, in `hermit-cli/tests/cli.rs`.
- Last retained green: `PASS 132.155s` at
  `992b7eb5b7922d55233ceda5a29afedcace03242`, in that run's retained
  `test.cli.log`.
- Findings: the retained green is far above the later 15-second cap. The a6
  observations therefore establish a bound kill, not a product verdict. Any
  debugging run must name which timeout configuration it used.

### `hermit::command_strict_verify$kernel_pseudofile_commands_are_deterministic_under_strict_verify`

- Observation: `FAIL 0.278s`, then `FAIL 0.274s`. Exact failure:
  `findmnt did not reach L2 under strict verification`; the protected
  comparison reported nondeterminism.
- Introduced: `2def556e3d50fae6d0f51a2fae11f400aa19782a`,
  2026-07-27, in `hermit-cli/tests/command_strict_verify.rs`.
- Last retained green: `PASS 1.735s` at
  `f77d7c44067a12ba11e75b5a85864ce0bc23e8f4`, in
  `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-105-f77d7c44067a-1787783997/dagrun/test.command_strict_verify.log`.
- Findings: this is a strict-verification failure in `findmnt`, not a timeout.
  No narrower retained root cause was found.

### `hermit::hermit_modes$verify_reports_exit_status_divergence`

- Observation: `FAIL 5.753s`, then `FAIL 0.208s`. Exact assertion:
  `assertion left == right failed: unexpected status`; observed left
  `Some(125)`, expected right `Some(1)`.
- Introduced: `3dbff1c4fa651b599570a8544fae970755eb014b`,
  2026-07-21, in `hermit-cli/tests/hermit_modes.rs`.
- Last retained green: `PASS 0.238s` at
  `55cb93a9fd76ddba7c9de853e2ce6bee012c1708`, run finished
  `2026-08-25T14:50:30Z`, in
  `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-004-55cb93a9fd76-1787667933/dagrun/test.hermit_modes.log`.
- Findings: the onset was narrowed to `556efd6f9e` against parent
  `9f258c6c64`, each checked three times. The verifier correctly finds the
  intended exit-status divergence, but returns Hermit's internal-failure status
  125 while the test still expects 1. Prior review explicitly rejected merely
  accepting 125 because verification divergence is a product result that needs
  the intended exit classification.

### `hermit::hermit_modes$verify_reports_stdout_divergence`

- Observation: `FAIL 0.261s`, then `FAIL 0.189s`; same exact status assertion,
  left `Some(125)`, right `Some(1)`. The expected stdout-mismatch diagnostic is
  present.
- Introduced: `3dbff1c4fa651b599570a8544fae970755eb014b`,
  2026-07-21, in `hermit-cli/tests/hermit_modes.rs`.
- Last retained green: `PASS 0.222s` at `55cb93a9fd76ddba7c9de853e2ce6bee012c1708`,
  in the same retained `test.hermit_modes.log`.
- Findings: same `556efd6f9e` onset and exit-classification issue as the
  preceding test; the comparator still detects the deliberately different
  stdout.

### `hermit::hermit_modes$verify_verbose_compares_the_full_trace`

- Observation: `FAIL 0.148s`, then `FAIL 0.121s`; same exact status assertion,
  left `Some(125)`, right `Some(1)`. The full-trace and nondeterminism
  diagnostics remain present.
- Introduced: `3dbff1c4fa651b599570a8544fae970755eb014b`,
  2026-07-21, in `hermit-cli/tests/hermit_modes.rs`.
- Last retained green: `PASS 0.148s` at `55cb93a9fd76ddba7c9de853e2ce6bee012c1708`,
  in the same retained `test.hermit_modes.log`.
- Findings: same `556efd6f9e` onset and product exit-classification question;
  do not weaken the full-trace comparison to clear this assertion.

### `hermit::liteinst_advanced$liteinst_fork_fails_closed_without_hanging`

- Observation: `FAIL 6.320s`, then `FAIL 6.521s`. Exact text includes
  `status=ExitStatus(unix_wait_status(32000))`, `LiteInst cancellation cleanup failed`,
  and `notifier did not acknowledge terminal cleanup`; left `Some(125)`, expected
  `Some(1)`.
- Introduced: `138922c767e2415851dea1201956cfd43b7867eb`,
  2026-07-26, in `hermit-cli/tests/liteinst_advanced.rs`.
- Last retained green: `PASS 6.618s` at
  `55cb93a9fd76ddba7c9de853e2ce6bee012c1708`, in
  `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-004-55cb93a9fd76-1787667933/dagrun/test.liteinst_strict.log`.
- Findings: reproduced with a matching release Hermit/runtime, so it is not the
  missing-revision staging problem. Activation succeeds, then the unsupported
  fork path reaches cancellation cleanup and that cleanup is not acknowledged.
  It remained after the `/proc/maps` fix in
  https://github.com/rrnewton/hermit/pull/2693. It is a backend-mechanism test,
  not a compatibility-corpus cell.
- **Renamed 2026-08-28, and the behaviour it asserted is gone.** This test is now
  `liteinst_fork_runs_without_hanging`. Reverie stopped refusing `clone`/`clone3`/`fork`
  under the LiteInst hybrid, so the assertion was updated from "the run is refused" to
  "the guest runs and prints `fork-ok`". The observations above are kept as written
  because they record what was measured at the time, under the old name and the old
  contract; they are not a claim about the current test. See
  https://github.com/rrnewton/reverie/pull/447 and
  https://github.com/rrnewton/hermit/pull/2816.

### `hermit::liteinst_advanced$liteinst_thread_clone_fails_closed_without_sigsys`

- Observation: `FAIL 2.487s`, then `FAIL 2.371s`. Exact text includes
  `status=ExitStatus(unix_wait_status(32000))` and `-524 ENOTSUPP`; left
  `Some(125)`, expected `Some(1)`.
- Introduced: `138922c767e2415851dea1201956cfd43b7867eb`,
  2026-07-26, in `hermit-cli/tests/liteinst_advanced.rs`.
- Last retained green: `PASS 2.540s` at `55cb93a9fd76ddba7c9de853e2ce6bee012c1708`,
  in the same retained `test.liteinst_strict.log`.
- Findings: same matched-runtime fail-closed boundary as the preceding test,
  without the missing-revision staging explanation. It is also a
  backend-mechanism test rather than a compatibility-corpus cell.
- **Renamed 2026-08-28, and the behaviour it asserted is gone.** This test is now
  `liteinst_thread_clone_runs_without_sigsys`. Reverie stopped refusing `clone`/`clone3`/`fork`
  under the LiteInst hybrid, so the assertion was updated from "the run is refused" to
  "the guest runs and prints `threads-ok`". The observations above are kept as written
  because they record what was measured at the time, under the old name and the old
  contract; they are not a claim about the current test. See
  https://github.com/rrnewton/reverie/pull/447 and
  https://github.com/rrnewton/hermit/pull/2816.

### `hermit::sabre_examples$sabre_non_racy_examples_verify_current_envelope`

- Observation: one `TIMEOUT 15.004s`; the node did not retry this ID, so there
  is one recorded attempt, not two, and no product assertion text.
- Introduced: `61d8df393b88e7654fd1bf6427c1f0abb381e696`,
  2026-07-31, in `hermit-cli/tests/sabre_examples.rs`.
- Last retained green: `PASS 12.814s` at
  `55cb93a9fd76ddba7c9de853e2ce6bee012c1708`, in
  `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-004-55cb93a9fd76-1787667933/dagrun/test.sabre_examples.log`.
- Findings: separate retained diagnosis shows a real SaBRe divergence in
  CPython shutdown/glibc arena trimming: the two runs had 2,989 versus 2,985
  syscall records and first differed at `madvise` address/length and virtual
  time. That rules out record loss, I/O-buffer content, native CPython, ptrace,
  and a minimal C allocator guest. A shared-address-space explanation remains
  unproved. Retained diagnosis directories are
  `/home/newton/work/dev-hermit/ignored/sabre-verify`, `sv-bin`, `sv-run1`,
  `sv-rep1`, `sv-rep2`, `sv-rep3`, `sv-noverify`, `sv-nv2`, `sv-c`, and
  `sv-native` below `/home/newton/work/dev-hermit/ignored/`.

### `hermit::signal_determinism$sigsuspend_without_signal_reports_terminal_deadlock`

- Observation: `FAIL 0.108s` twice. The scheduler emitted the intended terminal
  result, but the assertion saw left `Some(125)`, expected `Some(1)`.
- Introduced: `fa9a9418cb4fe99c762e05cbc57ff56728ce0b92`,
  2026-08-12, in `hermit-cli/tests/signal_determinism.rs`.
- Last retained green before the failing observation: `PASS 0.181s` at
  `55cb93a9fd76ddba7c9de853e2ce6bee012c1708`, in
  `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-004-55cb93a9fd76-1787667933/dagrun/test.hermit_integration.log`.
- Findings: the scheduler did not hang; the test had a stale exit-status literal
  after `556efd6f9e`. Commit `2dd8565bee2d13e7fa42edd1f5d6987f4e4191bf`
  replaced it with `HERMIT_INTERNAL_FAILURE_EXIT` and is on current main. No
  retained full-validate PASS after that fix was available when this guide was
  written.

## IDs with both outcomes at `a6b0c37648df`

### `hermit::app_strict_verify$python_is_deterministic_under_strict_verify`

- Observation: `FAIL 0.888s`, then `PASS 0.555s`. The failure says
  `hermit run --strict --verify was not deterministic (L2)`.
- Introduced: `fb573eed36adeb1bcf96ebfddf8e856f2807777e`,
  2026-07-28, in `hermit-cli/tests/app_strict_verify.rs`.
- Last retained green: `PASS 0.484s` at
  `f630aef3d18e87e49a1e099a5bcf4d2bf43987d1`, run finished
  `2026-08-27T10:51:24.977917Z`, in that run's retained
  `test.app_strict_verify.log`. The overall run was a non-verdict.
- Findings: fail/pass movement is established; no test-specific mechanism has
  been localized.

### `hermit::cli$run_dbt_verifies_process_wait_lifecycle`

- Observation: `PASS 0.987s`, then `FAIL 0.993s`; the failing strict DBT verify
  exited 125.
- Introduced under the current exact ID by the DBI-to-DBT rename
  `e565b1ab1d5fcbbab492ffb886c893dbcb061ddf`, commit date 2026-08-08
  (author date 2026-08-06), in `hermit-cli/tests/cli.rs`.
- Last retained green: `PASS 0.912s` at Hermit
  `4944fb5b3cc029459056a3b9743f0d0df3ad0209`, in
  `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-hermit-141-4944fb5b3cc0-1787834650301238559-798721-c47ac7c9/dagrun/test.cli.log`.
- Findings: the comparator was stable. DBT `wait4`/`waitid` fell back to
  host-timed `InternalIOPolling`, so physical child readiness changed protected
  scheduler records while guest stdout and status agreed. The repair landed via
  https://github.com/rrnewton/reverie/pull/506 and
  https://github.com/rrnewton/hermit/pull/2737, with Hermit merge
  `20226fd5d6fc221da8e4f58341fed411248b1995` on current main. The clean
  matched run at `4944fb5b3cc029459056a3b9743f0d0df3ad0209` passed in 0.912s
  and emitted no `UnexpectedEnd`. The two focused `6172478eae28` results do not
  describe authoritative current state because they used the stale packaged
  client described above; the Hermit source change prevents a same-SHA causal
  conclusion.

### `hermit::hermit_modes$verify_mode_matrix`

- Observation: `TIMEOUT 15.002s`, then `PASS 2.766s`, during the concurrent
  validate.
- Introduced: `53649eb86d812dcecc9cc1593c8e9f4f43c461da`,
  2026-07-21, in `hermit-cli/tests/hermit_modes.rs`.
- Last retained green: `PASS 2.954s` at
  `f630aef3d18e87e49a1e099a5bcf4d2bf43987d1`, in that run's retained
  `test.hermit_modes.log`; the overall run was a non-verdict.
- Findings: the two attempts show cap/pass movement. The test exercises the
  verify-mode matrix with `base-env=minimal`; no deeper test-specific cause was
  established.

### `hermit::liteinst_advanced$liteinst_strict_verify_python_random_example`

- Observation: `FAIL 2.847s`, then `PASS 3.039s`. The failing first verify run
  terminated with signal 11 (`SIGSEGV`) and the enclosing test observed status
  125.
- Introduced: `06f33c90a475b19c75b7c551d113e3bc65efecbe`,
  2026-07-30, in `hermit-cli/tests/liteinst_advanced.rs`.
- Last retained green: `PASS 3.342s` at
  `f630aef3d18e87e49a1e099a5bcf4d2bf43987d1`, in that run's retained
  `test.liteinst_strict.log`; the overall run was a non-verdict.
- Findings: it was never red in four pre-fix `/proc/maps` mutation runs, so the
  a6 failure must not be attributed to the padding defect fixed by
  https://github.com/rrnewton/hermit/pull/2693. No narrower cause is established.

### `hermit::liteinst_advanced$liteinst_strict_verify_round2_arithmetic_and_predicate_utilities`

- Observation: `TIMEOUT 15.002s`, then `PASS 5.690s`, during the concurrent
  validate.
- Introduced: `0c2de18cdbff2cd742d6d9e4ef1e33b3be764aa2`,
  2026-07-31, in `hermit-cli/tests/liteinst_advanced.rs`.
- Last retained green: `PASS 7.508s` at
  `f630aef3d18e87e49a1e099a5bcf4d2bf43987d1`; a later attempt in that same
  run timed out at `15.001s`, and the run itself was a non-verdict.
- Findings: repeated cap/pass movement is established; no product failure text
  or narrower cause is retained.

### `hermit::liteinst_advanced$liteinst_strict_verify_round3_stdin_filter_utilities`

- Observation: `TIMEOUT 15.002s`, then `PASS 4.083s`, during the concurrent
  validate.
- Introduced: `57a3ea41b9a474f7311aaaa643583ade1cc7f5a3`,
  2026-07-31, in `hermit-cli/tests/liteinst_advanced.rs`.
- Last retained green: `PASS 8.573s` at
  `f630aef3d18e87e49a1e099a5bcf4d2bf43987d1`, in that run's retained
  `test.liteinst_strict.log`; the overall run was a non-verdict.
- Findings: cap/pass movement is established; no deeper per-ID cause was found.

### `hermit::liteinst_advanced$liteinst_strict_verify_semantic_text_utilities`

- Observation: `PASS 6.726s`, then `TIMEOUT 15.003s`, during the concurrent
  validate. This ID appeared in the old final-red list only because that reader
  kept the final attempt; order-independent classification puts it here.
- Introduced: `4867b52f2d180648968e9449f43fc9d20dac5935`,
  2026-07-31, in `hermit-cli/tests/liteinst_advanced.rs`.
- Last retained green: `PASS 6.807s` at
  `f630aef3d18e87e49a1e099a5bcf4d2bf43987d1`, in that run's retained
  `test.liteinst_strict.log`; the overall run was a non-verdict.
- Findings: this family once diverged through raw `/proc/maps` padding, fixed by
  https://github.com/rrnewton/hermit/pull/2693. The a6 non-pass is a timeout,
  so it must not be relabeled as that older divergence.

## How to continue

For a new run, classify each exact `BINARY$TEST` ID by all of its attempt
verdicts. Do not use the last verdict, and do not count nextest's repeated
failure recap as another attempt. Retry occurrence counts come from non-null
`attempts[].retry_class`, not `retried_nodes`, because `retried_nodes` includes
peer nodes rerun with a failing node. Treat `retry_class` as the grouping key;
changing evidence such as a measured pass/fail sample is in the adjacent
`retry_detail` field.

When a current full validate can pass `pre.reverie_pin`, replace the focused
28-ID result with a full-population classification. Preserve the host, kernel,
SHA, concurrency, timeout, and attempt-count conditions; do not compare raw node
counts across hosts with different `cpuid_fault` capability.
