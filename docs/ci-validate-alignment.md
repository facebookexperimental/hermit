<!--
Copyright (c) Meta Platforms, Inc. and affiliates.
All rights reserved.

This source code is licensed under the BSD-style license found in the
LICENSE file in the root directory of this source tree.
-->

# CI ⇄ `validate.sh` Alignment

This document is the authoritative mapping between the fork's GitHub Actions
workflow (`.github/workflows/ci.yml`) and the local validation script
(`validate.sh`). It exists so that **every test runs in both places, in the
same mode**, and so that a reviewer can confirm at a glance that a
test-adding PR did not update only one of the two.

The invariant we maintain:

> Every check that gates a merge in CI must be reproducible locally with
> `./validate.sh`, and every check `validate.sh` runs must also gate CI.
> When the two legitimately differ (host capability), the difference is
> listed explicitly below and nowhere else.

`validate.sh` is the local equivalent of the **entire** CI matrix, i.e. the
`regular` job **plus** the `hardware` job. CI splits the matrix into two jobs
only because GitHub-hosted runners lack a Performance Monitoring Unit (PMU)
and mount-namespace privileges; a developer machine that has both runs the
whole thing in one `./validate.sh` invocation.

## Why there are two CI jobs

| CI job | Runner | Has PMU / namespaces? | Purpose |
| --- | --- | --- | --- |
| `regular` | `ubuntu-latest` (GitHub-hosted) | No | Build, lint, format, docs, and all host-independent unit/integration tests. |
| `hardware` | self-hosted `[Linux, X64, hermit, pmu]` | Yes | PMU-backed determinism, record/replay, mount-namespace integration tests, and the working-envelope gate. |

`validate.sh` does not have this split: it assumes the developer host has PMU
and namespace support and runs both tiers. Checks that need hardware the host
lacks fail loudly rather than silently skipping (see "Host-capability
differences").

## Mapping table

Status legend: ✅ identical / superset · ⚠️ same test, different mode ·
❌ present in one side only (gap).

### Host-independent checks (CI `regular` job)

| CI `regular` step | Command | `validate.sh` counterpart | Status |
| --- | --- | --- | --- |
| Build | `cargo build --workspace` | "Build workspace" — `cargo build --workspace` | ✅ |
| Test regular workspace crates | `cargo nextest run --profile ci --workspace --exclude detcore --exclude hermit --exclude hermetic_infra_hermit_flaky-tests` | "Test workspace and integrations" — `cargo nextest run [--profile ci] --workspace --exclude detcore --exclude hermetic_infra_hermit_flaky-tests` (also includes `hermit`) | ✅ superset (validate additionally runs `hermit` integration tests here) |
| Test Hermit (no namespace tests) | `cargo test -p hermit --lib --bins` | covered by validate's workspace nextest run | ✅ |
| Test Detcore (no hardware tests) | `cargo test -p detcore --lib --bins` + `tests_misc getrandom_intercepted --exact` | "Test detcore package" — `cargo test -p detcore` | ✅ superset |
| Documentation | `cargo test --workspace --doc` + `cargo doc --workspace --no-deps` | "Test workspace documentation" + "Documentation" | ✅ |
| Clippy | `cargo clippy --workspace --all-targets -- -D warnings` | "Clippy" | ✅ |
| Format | `cargo fmt --all -- --check` | "Rustfmt" | ✅ |

`validate.sh` selects the `ci` nextest profile only when `CI` is set in the
environment; locally it uses the default profile. The set of tests selected is
identical; only reporting (JUnit output, retries) differs.

### Host-dependent checks (CI `hardware` job)

| CI `hardware` step | Command | `validate.sh` counterpart | Status |
| --- | --- | --- | --- |
| CPUID and RDRAND/RDSEED | `cargo test -p detcore --test tests_misc has_rdrand_without_detcore \| rdrand_rdseed_is_masked -- --exact` | "Test detcore package" runs these (non-ignored) | ✅ |
| PMU timing and misc | `tests_misc getrandom_intercepted --exact`; `cargo test -p detcore --test tests_time -- --ignored --test-threads=4` | "Test detcore package" runs `tests_time` non-ignored | ⚠️ **and CI bug** — see note (1) |
| PMU parallel futex | `cargo test -p detcore --test tests_parallelism futex_wait_parent -- --ignored --test-threads=3` | "Test detcore package" runs it non-ignored | ⚠️ note (1) |
| PMU parallel memory | `cargo test -p detcore --test tests_parallelism 'mem_race::' \| 'mem_print_race::' -- --ignored --test-threads=4` | "Test detcore package" | ⚠️ note (1) |
| Stable Hermit tests incl. record/replay matrix | serial loop over `arbitrary_binaries, cli, clock_determinism, epoll_determinism, mmap_determinism, procfs_determinism, signal_determinism` (`--test-threads=1`) + `record_replay_matrix` + `strict_mode_matrix` | validate runs the same tests via workspace nextest (parallel processes) | ⚠️ note (2) |
| Fail-closed ratchet | `./scripts/test-fail-closed.sh` | — | ❌ gap A |
| Working-envelope gate | `./validate.sh --envelope-compare envelope-baseline.json` | validate's `run_envelope` (measure, no compare) | ✅ shared code path by construction |
| Debugger integration | `./tests/debugger/run_debugger_tests.sh` | — | ❌ gap B |
| Backend parity ratchet | `python3 experiments/backend-parity_20260722/run_matrix.py …` | — | ❌ gap C |

### Checks only in `validate.sh`

| `validate.sh` step | Command | CI counterpart | Status |
| --- | --- | --- | --- |
| Hermit run smoke test | `hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled -- /bin/echo …` | envelope L1 (`--strict`) approximates it | ⚠️ partial |
| Hermit output determinism | run twice, diff stdout | — | ❌ gap D |
| Hermit verify-mode smoke test | `hermit run … --verify -- /bin/echo …` | envelope L2 (`--strict --verify`) approximates it | ⚠️ partial |
| Fast concurrency stress suite | `cargo nextest run -p hermit --test stress_suite --run-ignored only -E 'test(=fast_chaos_matrix)'` | — | ❌ gap E |
| Hermit analyze scenarios | `cargo test -p hermit --test analyze -- --ignored` | — | ❌ gap F |
| Schedule search E2E | `./tests/util/hermit_analyze_e2e.sh` | — | ❌ gap G |

## Notes

**(1) CI `--ignored` on detcore PMU tests is a silent no-op on `main`.**
`detcore/tests/time/mod.rs` and `detcore/tests/parallelism/mod.rs` contain
**no** `#[ignore]` attributes, so `cargo test … -- --ignored` (which selects
*only* ignored tests) currently runs **zero** tests in the "PMU timing",
"PMU parallel futex", and "PMU parallel memory" steps. `validate.sh` runs the
same test binaries *without* `--ignored` and therefore actually exercises them.
The fix is to drop `--ignored` from those CI steps (the `frontier` branch and
PRs #152–#154 already do this). Until then, `validate.sh` has strictly more
detcore coverage than CI here.

**(2) Serial vs. parallel execution of PMU-sensitive Hermit tests.**
The `hardware` job runs the determinism integration tests with
`--test-threads=1` because they contend for the PMU. `validate.sh` runs them
through `cargo nextest`, which launches each test in its own process (still
one guest at a time per process, but multiple processes concurrently). The
*set* of tests is the same; a flake that only appears under one scheduling
should be reproduced with the matching invocation before diagnosing.

## Gap ledger (must be driven to empty)

| ID | Check | Lives in | Action to align |
| --- | --- | --- | --- |
| A | `scripts/test-fail-closed.sh` | CI only | Add a `run_check "Fail-closed ratchet" ./scripts/test-fail-closed.sh` step to `validate.sh`. |
| B | `tests/debugger/run_debugger_tests.sh` | CI only | Add a `run_check` step to `validate.sh`. |
| C | Backend parity ratchet | CI only | Add a `run_check` invoking `run_matrix.py --backend ptrace` to `validate.sh`. |
| D | Hermit output determinism | validate only | Add an equivalent probe/step to the CI `hardware` job (or fold into the envelope gate). |
| E | `fast_chaos_matrix` | validate only | Add a CI `hardware` step: `cargo nextest run -p hermit --test stress_suite --run-ignored only -E 'test(=fast_chaos_matrix)'`. |
| F | `analyze` scenarios | validate only | Add a CI `hardware` step: `cargo test -p hermit --test analyze -- --ignored`. |
| G | Schedule search E2E | validate only | Add a CI `hardware` step running `./tests/util/hermit_analyze_e2e.sh`. |

The `main` ↔ `frontier` divergence is itself the largest single source of
skew: `frontier` has already removed the envelope/debugger/backend-parity
steps and added `rr_suite` + LevelDB steps. Whichever shape `main` converges
to, this table must be updated in the same PR so it never lies.

## Reconciliation checklist for test-adding PRs

Any PR that adds, removes, or renames a test **must** keep the two sides in
sync. Before requesting review, confirm:

1. The new test is invoked in **both** `.github/workflows/ci.yml` **and**
   `validate.sh` (unless it is genuinely host-only — then it goes only in the
   CI `hardware` job *and* is listed in the "Host-capability differences"
   section below, never silently).
2. The invocation **mode matches**: same package, same `--test`/`-E` filter,
   same `--ignored` / `--run-ignored` selection, same `--test-threads`.
3. This document's mapping table is updated in the same PR.
4. If the test contributes to the working envelope, `envelope-baseline.json`
   is raised (never lowered) and `./validate.sh --envelope-compare
   envelope-baseline.json` still passes.

### Known in-flight PRs (as of 2026-07-22)

These open PRs all move the test matrix and must be reconciled against this
table when they land (all target `main` except #155):

- **#152** `impl-validate-strict-mode` — runs all `validate.sh` **and** CI
  hermit invocations under `--strict`. Touches both files; keeps them in sync.
- **#153** `strict-workload-matrix` — adds a real strict-determinism workload
  matrix. Touches both files.
- **#154** `impl-fbsource-replay-matrix` — adds
  `hermit-cli/tests/replay_matrix.rs` (`trace_replay_matrix`,
  `chaos_replay_matrix`) and wires it into CI **but does not modify
  `validate.sh`**. This introduces gap-shaped skew (a CI-only test); align by
  adding the same invocation to `validate.sh` before or when it lands.
- **#155** `fix-fbsource-chaos-matrix` — expands the verified chaos matrix in
  `hermit_modes.rs`. Targets `frontier`, not `main`.

## Host-capability differences (the *only* sanctioned skew)

A check may run in CI's `hardware` job but not on a given developer host, or
vice versa, **only** for a hardware/privilege reason, and it must be named
here:

- PMU (retired-conditional-branch counters) — required by determinism and
  record/replay tests. Absent in most VMs and GitHub-hosted runners, present
  on the self-hosted `pmu` runner.
- Mount namespaces (`unshare --user --map-root-user --mount …`) — required by
  the Hermit integration tests. The CI `hardware` job requires them; a host
  without them cannot run those tests under `validate.sh` either.
- CPUID interception — the `tests_misc` RDRAND/RDSEED tests depend on it and
  can fail on VMs that expose unusual CPU features.

No skew other than the above is acceptable; anything else is a gap in the
ledger and must be closed, not documented away.
