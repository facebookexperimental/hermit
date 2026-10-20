# Testing Environments: VM, Container, and Bare-Metal Expectations

Hermit documents **x86-64 Linux** as its supported platform, but the
architecture alone does not predict whether deterministic execution or the full
Cargo test suite will pass. Whether a given test runs depends on several
*independent* host capabilities — CPU model, hardware performance counters
(PMU), `perf_event_open` permissions, CPUID interception, and user/mount
namespaces. This document is the environment contract: it says which tests need
which capabilities, where each test tier is expected to run, what an
environment-related failure looks like, and how to tell an environment problem
apart from a genuine Hermit or Reverie bug.

This document is **documentation only**. It does not add capability detection or
conditional test execution; if those are wanted, track them in a separate
implementation issue and link it here.

> The headline rule: **not every x86-64 Linux machine can run every Hermit
> test.** A green `cargo test --workspace` on a portable VM demonstrates the
> environment-independent subset only, not the PMU- and namespace-dependent
> integration matrix.

## `scripts/validate.rs` levels

`./scripts/validate.rs` accepts one optional validation level. With no level argument,
it runs `full` for backward compatibility.

| Level | Typical estimate | Coverage |
| --- | --- | --- |
| `quick` | About 3 minutes | Builds the workspace, runs Detcore's core unit tests, and exercises ptrace run, repeat-output, verify, record, and replay smoke tests. It does not execute DBT or KVM or build the optimized binary. |
| `portable-only` | About 8 minutes | Constructs the portable plan also selected by the integration-branch and manually dispatched GitHub-managed portable workflow: build, portable workspace tests, Hermit and Detcore library/binary tests, docs, Clippy, and rustfmt. It does not require PMU or guest namespaces. |
| `full` (default) | About 20-70 minutes | Constructs the portable and privileged plan from the committed validation data. This includes the portable product gates plus the focused CPUID, PMU, KVM, and record/replay capability partition. |
| `super` | About 30-90 minutes | Builds Hermit and repeats each bounded determinism probe 20 times by default. It reports `passed/total` for every probe and fails if any iteration fails. Available KVM and DBT verify probes join the ptrace strict-verify, pipeline, and record/replay probes. |

Select a level positionally or with `VALIDATE_LEVEL`. The long-form aliases are
useful in scripts and make the intended capability tier explicit:

```sh
./scripts/validate.rs --quick
./scripts/validate.rs --portable
VALIDATE_LEVEL=portable-only ./scripts/validate.rs
```

`--quick` is an alias for `quick`; `--portable` and `--portable-only` are aliases
for `portable-only`. The script prints the selected profile and its estimate
before starting any gate. Treat estimates as planning guidance: a cold Cargo
cache and host contention can increase elapsed time.

Super mode defaults to `SUPER_REPETITIONS=20` and a concurrency limit of about
1.5 times the online CPU count. Override those with positive integers in
`SUPER_REPETITIONS` and `SUPER_JOBS`; lower values are useful for a local smoke
run. The runner prints the host OS from `/etc/os-release`, repetition count,
concurrency, and online CPU count so pass-rate reports retain their execution
context.

### Relaxed-mode flag matrix

The `super` tier includes an occasional ptrace matrix that checks every
meaningful combination from strict deterministic execution through the
passthrough endpoints. The 60-state cross-product is:

| Axis | States |
| --- | --- |
| Policy | relaxed; strict (which requires sequentialization and deterministic I/O) |
| Thread scheduling | sequentialized; non-sequentialized (relaxed only) |
| I/O | deterministic; host behavior (relaxed only) |
| Time and metadata | both virtualized; time only; neither (metadata cannot be virtualized without time) |
| CPUID | virtualized; host behavior |
| Verification | off; two-run verification on |

Three endpoint configurations add `--strace-only`, `--strace-only --verify`,
and `--namespace-only`. Verification is invalid with `--namespace-only` and is
therefore not presented as a runnable state. Each of the 63 configurations runs
`/bin/true`, fixed stdio, and a threaded workload that observes clocks, file
metadata, CPUID, and randomness: 189 bounded cases in total.

Run the matrix directly with:

```sh
HERMIT_FLAG_MATRIX_REPORT=target/relaxed-flag-matrix/results.tsv \
  cargo test -p hermit --test relaxed_flag_matrix \
  meaningful_flag_combinations_run_without_crashing -- \
  --exact --ignored --test-threads=1 --nocapture
```

The TSV report records the configuration, program, verification setting,
outcome, and elapsed milliseconds. A valid non-verifying run must exit zero.
A verifying run must either carry Hermit's deterministic success marker or its
explicit nondeterminism result; timeouts, signals, panics, and unexplained
nonzero exits fail the test. This distinction makes relaxed and passthrough
verification useful without pretending those modes promise determinism.

The exact strict/no-time `clock_gettime` failure is temporarily ratcheted as an
expected failure linked to [issue #1176](https://github.com/rrnewton/hermit/issues/1176).
Only that diagnostic signature is accepted, and the test requires its expected
count so a fix prompts removal of the exception instead of silently leaving a
stale waiver.

The full and super backend gates probe actual runtime capability: KVM requires
a readable and writable `/dev/kvm`; DBT must complete a bounded `/bin/true`
smoke using either its bundled DynamoRIO runtime or explicit environment
configuration. An unavailable alternate backend is reported as `SKIP`, not as
a ptrace failure.

The full compatibility matrices invoke system utilities by their Linux paths
or command names. Ubuntu and Fedora split those tools across different
packages (notably `coreutils`, `util-linux`, `bsdextrautils`/`util-linux`, and
compression packages). The OS name printed at startup must accompany results;
install missing utilities rather than interpreting `command not found` as a
Hermit determinism regression.

## Capability axes

These axes are orthogonal. A host can satisfy some and not others, and each
gates a different set of tests.

| Axis | What it means | How to check |
| --- | --- | --- |
| **Architecture support** | Hermit targets x86-64 Linux. AArch64 is incomplete; macOS is unsupported. | `uname -m` reports `x86_64` |
| **CPU-model support** | The Reverie timer/perf layer must recognize the specific processor model. Newer CPUs can still be rejected even on bare metal. | `lscpu`; watch for timer/perf errors at startup |
| **PMU availability** | Deterministic preemption counts **retired conditional branches (RCBs)** via the CPU performance-monitoring unit. Many VMs and restricted containers do not expose a usable PMU. | `perf stat -e branches true`; see failure signatures below |
| **perf permissions** | Even with a PMU, the kernel must allow `perf_event_open` for the user. | `cat /proc/sys/kernel/perf_event_paranoid` (lower is more permissive; `<= 1` is typically required for unprivileged use) |
| **CPUID interception** | RDRAND/RDSEED masking and CPUID virtualization rely on CPUID faulting. Some virtualized hosts prevent the fault from taking effect or expose unexpected feature bits. | Requires CPUID-faulting support; see `rdrand_rdseed_is_masked` below |
| **User/mount namespaces** | Hermit builds the guest container from user, mount, PID, and UTS namespaces. Many container runtimes and hardened hosts block these. | `unshare --user --map-root-user --mount true` |

Because the axes are independent, describe an environment by its capabilities,
not by a single "supported/unsupported" label.

## Environment support matrix

Expected outcome of the public Cargo suite per environment. "Env-independent
subset" = the crates and tests that do **not** require PMU, CPUID faulting, or
namespaces (see the [CI tiers](#ci-tiers-what-runs-where) below).

| Environment | Arch | CPU-model | PMU | perf perms | CPUID faulting | Namespaces | Expected outcome |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **Bare metal (supported CPU)** | ✅ | ✅ (if recognized) | ✅ | ✅ (if `perf_event_paranoid` permits) | usually ✅ | ✅ | Full suite can pass, including PMU + namespace integration tests |
| **Bare metal (newer/unrecognized CPU)** | ✅ | ❌ maybe | ✅ | ✅ | ✅ | ✅ | Timer/perf layer may reject the model → PMU tests fail; file a CPU-support bug (see below) |
| **Portable VM (typical cloud)** | ✅ | varies | ❌ usually | n/a | ❌ often | often ✅ | Env-independent subset passes; PMU and CPUID/RDRAND tests fail or skip |
| **Privileged VM with virtualized PMU** | ✅ | ✅ | ✅ (if configured) | ✅ | varies | ✅ | Approaches bare metal; validate PMU tests explicitly before trusting them |
| **Container (shares host CPU)** | ✅ | inherits host | inherits host, but | ❌ often restricted | inherits host | ❌ often blocked | CPU/PMU capabilities come from the host, but perf perms and namespaces are commonly restricted independently |
| **WSL** | ✅ | varies | ❌ usually | n/a | varies | varies | Treat like a portable VM; PMU-dependent tests are not expected to pass |

Notes:

- A container **shares the host's physical CPU and PMU**, but the runtime and
  kernel config can still block `perf_event_open` and namespace creation, so a
  container on a PMU-capable host is not automatically PMU-capable for tests.
- "Skip" vs "fail": some hardware-sensitive tests now guard their prerequisites
  and print a skip message instead of failing (see
  [Hardware-sensitive tests](#hardware-sensitive-cargo-tests)). Older reports of
  hard failures on VMs may predate those guards.

## CI tiers: what runs where

Local exact-head `scripts/validate.rs` is the canonical validation and landing
path. The portable workflow also runs automatically after pushes to
`integration`; that run is a second signal and never gates a pull request or a
landing. Portable and privileged workflows remain manually dispatchable for
comparing an ordinary GitHub Actions runner with a capability-bearing runner:

### `regular` — GitHub-managed portable (`ubuntu-latest`)

Runs after a push to `integration` or by `workflow_dispatch`. It asks
`scripts/validate.rs` for selected step tags from the constructed portable plan and covers the
**environment-independent subset**:

- `cargo build --workspace`
- `cargo nextest run --profile ci --workspace` **excluding** `hermit-detcore`,
  `hermit`, and `hermetic_infra_hermit_flaky-tests`
- `cargo test -p hermit --lib --bins` (no namespace-dependent integration tests)
- `cargo test -p hermit-detcore --lib --bins`
- doc tests (`cargo test --workspace --doc`), `cargo doc`, Clippy, rustfmt

GitHub-managed portable runners have **no usable PMU and no CPUID faulting**, so the
detcore and hermit integration suites are deliberately excluded here.

They also have **no dev-hermit parent repository**: the checkout is bare Hermit.
One checker needs that parent, the way a KVM test needs `/dev/kvm`: the accept
arm of the canonical ledger adapter contract in
`scripts/test_validate_stop_paths.py`, which drives the parent's real
`ci-hub/ledger/validate_rows.py`. It is not run in this tier and is not counted
as passed here. `check.lint_checks` runs that file with
`--exclude-canonical-adapter-accept-arm`, which prints `NOT COVERED BY THIS RUN`
and names the node that does cover it: `check.canonical_adapter_accept`,
labelled `full` only, so it runs in local `scripts/validate.rs full` from a
checkout nested under the parent. From a checkout without the parent that node
reports NO RESULT (exit 75), never a pass. `make lint-parent-checks` runs the
same arm by hand.

### `privileged` — capability runner (`[Linux, X64, hermit, pmu]`)

Runs only by `workflow_dispatch`.
Requires PMU access, CPUID faulting, and read/write `/dev/kvm`. It is a focused
sub-five-minute sentinel: one CPUID test, one direct PMU overflow/skid probe,
and one KVM multi-mode E2E cell. Broad product and stress coverage remains in
portable or local validation.

## Named measurement hosts

`scripts/check-portable-paths.sh` refuses a literal hostname in a file that
**builds or runs** — its scope predicate is `is_build_or_run_file`, and its own
self-test pins that arbitrary text evidence is deliberately outside it. The harm it
exists to prevent is a build or a run breaking on another machine; prose cannot do
that. So this file is out of scope BY DESIGN, and that is what makes it the right
home for a host identity.

⚠️ **"The scanner does not look here" would NOT be a good enough reason.** Unscanned
is not the same as permitted, and treating it as such is how a rule gets evaded by
relocation. The claim above is the stronger one — the scope is deliberate, named and
tested — and `scripts/check-portable-paths.sh` names this section as the designated
destination, with a self-test, so the permission is declared rather than inferred.

A measurement whose host is unrecorded cannot be re-run, compared, or challenged.
Four fixes on 2026-08-25 (#2646, #2647, #2648, #2652) each satisfied the checker by
erasing the host; this table is where the erased identities go back.

| lane / node | measured on | what was measured |
| --- | --- | --- |
| `FINALIZE_BUDGET` in `hermit-cli/src/bin/hermit/owned_container.rs` | `devbig030` | Publication-to-reap latency of trace-level `hermit run --record-preemptions-to` of rustbin_heap_ptrs through safehermit, 10,782 runs at 1-1264 concurrent: max 1.52s (316 concurrent plus 316 CPU stress workers); https://github.com/rrnewton/hermit/issues/3414 |
| `privileged` lane, node `test.cli_kvm` | `devbig014` | `/dev/kvm` mode 666; `open(O_RDWR)` succeeds; `KVM_GET_API_VERSION` = 12; `hermit --backend kvm run` exits 0 |
| `privileged` lane, cell `applications/kvm-python-examples` | `devbig014`, `devbig030` | Retained passing validations measured 14.87-56.51 seconds; the same SHA measured 16.29 and 39.85 seconds respectively, so 15-second and 30-second bounds both cut valid L2 work |
| `scripts/check-reverie-pin.rs` git-env lock | `devbig014` | 30x no-guard run failed 0 times; mutation of `under_git_env` failed 9 of 10 at load average ~39 |
| `scripts/bisect-probe.rs` cost split | `devbig014` | BUILD 36.33s (hermit binary 36.21 + guest 0.12/id) vs TEST 3.53s per cell, range 1.6-10.9; `run --prebuilt` 6 cells 21.76s serial vs 11.55s at `--jobs 6`; 237 portable test ids expand to 304 required cells |
| validation DAG node `gate.manifest` and the five `selftest.*` nodes split from it (descriptions in `ci/manifest-plan/src/validation_dag_static.rs`) | `devbig014` | On 2026-09-29 between 20:39Z and 21:05Z, at Hermit 98a621f4 plus https://github.com/rrnewton/hermit/issues/3381, at load average 215-233: `test-harness validate` took 2.88 s wall and 2.53 CPU seconds, and the tool self-test timings quoted in each node description |
| pinned authority outage, validation DAG node `check.lint_checks` | `devbig014` | On 2026-09-04, an HTTP 504 fetching the check-status authority made `make lint-checks` exit 2 and the node report FAILED; an unreachable review-label contract raised `RuntimeError` and made `make lint-checks` exit 1. Two gates went red on four consecutive hourly runs of a tree that had passed 267/267 earlier that day. |
| pre-push submodule diagnosis | `devbig014` | On 2026-09-04, a fresh detached worktree with unpopulated submodules made Cargo fail before linting a Python-and-Makefile-only change; the hook incorrectly described that as a compile failure until the diagnosis was made explicit. |
| `portable` lane, node `test.hermit_unit` | `devbig030` | Warm repeats at `a6b0c37648df`: nextest `-j1` 27.0s and 27.3s; `-j16` 14.4s and 13.7s; `CARGO_BUILD_JOBS=8` unchanged |
| `refusal_detail_tests::REAL` in `scripts/validate.rs`, run 1838 | `devbig014` | On 2026-09-17, validation of `158a89f6217b25db9540237f9c1e256cdbaf785c` recorded `parity history changes candidate identity` for `portable/backend-parity-c/backend-parity-c/aio-refusal/verify@kvm`, run `validate-ops-tick-158a89f6217b-088d91951315`, outer attempt 2. The original 407-byte diagnostic is preserved unchanged in `tests/fixtures/scorecard-writeback/refusal.txt` and included as test data; its historical artifact path is evidence, not a runtime filesystem dependency. |
| `privileged` lane, node `privileged-test.pmu_ptrace_completion_cases` | `devbig014` | The five `ptrace_completion::tests::real_random_` cases pass in 1.94 s wall with a 33 MB peak RSS |
| `privileged` lane, node `privileged-test.pmu_cli_cases` | `devbig014` | `skid_overshoot_and_guest_failure_have_different_exit_codes` passes in 0.44 s. On 2026-09-29, after the preempted nonleader-exec fixture switched to PAUSE spin loops, nextest took 23.2-30.7 s for all five of the node's cases over 20 runs, at one-minute load averages of 63-226; `run_ptrace_nonleader_exec_preserves_preemption` used 5.95-7.48 CPU-seconds of its 22 |
| validation CPU placement, `scripts/lib/safe_ci_scope.rs` (https://github.com/rrnewton/hermit/issues/3265) | `devbig014` | A ptrace-stop-heavy cell costs 7.6-7.9x the CPU on the CPUs the AMD uncore and L3 PMUs are bound to |
| `portable` cell `data-handling/dd-partial-transfers` payload, `tests/e2e/manifests/data-handling.yaml` (https://github.com/rrnewton/hermit/issues/3337) | `devbig014` | 2026-09-28 at Hermit `ad21724d5f64`, ptrace verify: payload 4096 -> 512 bytes; debug median CPU 18.59 s -> 4.76 s, release 4.39 s -> 1.14 s over 5 runs each |
| `privileged` lane, cell `system-utils/sysfs-sanitized-prefixes`, nodes `privileged-e2e.manifest_system_utils` and `privileged-only-e2e.manifest_system_utils` (plus `_on_host`) | `devbig014` | Wall per run from the validation ledger series, 2026-08 to 2026-09: verify/ptrace 148 samples, median 2096 ms, max 15438 ms; verify/kvm 29 samples, median 2316 ms, max 3467 ms. This is the evidence cited by the nodes' descriptions in `ci/manifest-plan/src/validation_dag_static.rs` |
| scorecard parity fixture `ci/compat-envelope/testdata/parity-ledger/` (https://github.com/rrnewton/hermit/issues/3301) | `devbig014` | Its source, validate run `validate-coord-s17-d3a0a4ae3d16-1790649970094317174-3328074-da07a1da` at Hermit `d3a0a4ae3d16`, is retained there under dev-hermit `ignored/validate/artifacts/<run_id>/` and was never published: 190 portable and 2 privileged parity records. The fixture's own `host` value is the neutral `fixture-host-b` (see its `provenance.json`) |
| patchelf fallback in `ci/buck-e2e/stage` | `devbig030` | On 2026-10-02 (commit `ab5f0c8ec33`) the host had no patchelf package, so staging took the fallback: the static binary from the pinned PyPI wheel `patchelf==0.19.1.0`, which printed `patchelf 0.19.1`; with a wrong pinned hash the step exits 1 with "does not match its pinned sha256" |
| `_FRAME_IDENTITY_DEPTH` in `hermit-cli/src/bin/hermit-dap-reverse.py` (https://github.com/rrnewton/hermit/pull/3545) | `devbig030` | GDB 17.2, Reverie `4f125805`, commit `7006270446e`: one `continue` past a recursion 1000 calls deep took 121.79 s when the frame identity unwound every frame and 4.77 s with the newest 8; at 300 calls deep, 12.15 s and 1.59 s |
| `DAP_HELPER_CHAIN_SOURCE` in `hermit-cli/tests/cli.rs` (https://github.com/rrnewton/hermit/pull/3545) | `devbig030` | GDB 17.2, commit `f4055eb9cae`, checked with `bt` and `gdb.Frame.read_register("sp")`: at line 5 of `h0`, frames 0 to 6 have the same pc and sp in both activations; only frame 7, `main`, differs, 0x4011e9 against 0x4011f6 at the same sp, 0x7fffffffb060 |
| `hermit_dap_replay_continue_through_deep_recursion_is_fast_and_lands_exactly` in `hermit-cli/tests/cli.rs` (https://github.com/rrnewton/hermit/pull/3545) | `devbig030` | GDB 17.2, Reverie `4f125805`, one test thread, commit `f4055eb9cae`: the test's one `continue` (about 2000 line arrivals, depth up to 1000) took 2.51 s to 2.66 s in eight runs with the newest 8 frames, and 63.45 s with every frame |
| `portable` lane, node `test.detcore_time` (description in `ci/manifest-plan/src/validation_dag_static.rs`) | `devbig014` | 2026-09-29 at Hermit `905903e0a7ed`: five serial runs of `cargo nextest run -p hermit-detcore --test tests_time -j 1`, each in its own scope, ran 28 of 28 tests; cgroup memory.peak 111661056-115081216 bytes and -j1 wall 5.32-6.12 s, the figures behind the node's memory baseline and est_duration_s. 2026-10-03 at Hermit `c3f7b2bcfa00`, after the node kept only the two cases that disable `max_timeslice` (https://github.com/rrnewton/hermit/issues/3663): five runs of the node's filter ran 2 tests with 29 filtered out in 1.09-1.20 s wall, largest process 96-106 MB RSS |
| local `full` validation only (label `full`; not selected by `.github/workflows/ci-privileged.yml`), node `privileged-test.pmu_detcore_time_cases` (description in `ci/manifest-plan/src/validation_dag_static.rs`) | `devbig014` | 2026-10-03 at Hermit `c3f7b2bcfa00`: five runs of `cargo nextest run -p hermit-detcore --test tests_time -j 1` with the node's filter ran the 29 cases that enable `max_timeslice`, with 2 filtered out, in 4.72-5.90 s wall (4.15-5.00 s of nextest time), largest process 68-76 MB RSS |
| `SUPER_PROBE_MEM_BYTES` in `scripts/lib/validate_super.rs` | `devbig030` | Super stress probes, 20 repetitions at once, each Hermit call in its own safehermit cgroup: per-repetition peaks summed to 1052, 1226 and 1433 MiB for the strict, pipeline and record probes (largest single repetition 73 MiB); the KVM probe with the release binary summed to 1363 MiB (largest 71 MiB) |
| `SABRE_PLUGIN_GLIBC_MINOR_FLOOR` in `ci/publish-hermit-e2e-artifact.sh` and `ci/verify-hermit-e2e-artifact.sh` (https://github.com/rrnewton/hermit/issues/3652) | `devbig030` | The oldest supported host: glibc 2.34, kernel 7.1.3. On 2026-10-03 the SaBRe plugin built in the pinned root (glibc 2.42) required GLIBC_2.38 and did not load into guests there; at 4e05a36ee985 the plugin requires nothing newer than GLIBC_2.34 and `--backend sabre run` of `/usr/bin/echo` exits 0 with `reach_state=sabre-exercised` |

## Hardware-sensitive Cargo tests

Named tests and the capabilities they require. Paths are relative to the repo
root.

| Test / group | File | Requires | Notes |
| --- | --- | --- | --- |
| `has_rdrand_without_detcore` | `detcore/tests/misc/mod.rs` | Host RDRAND | Probes host features; returns early if RDRAND absent |
| `rdrand_rdseed_is_masked` | `detcore/tests/misc/mod.rs` | RDRAND/RDSEED **and** CPUID faulting | Runs without PMU (`det_test_fn_without_pmu`); skips if faulting unsupported |
| `getrandom_intercepted` | `detcore/tests/misc/mod.rs` | None (PMU-free) | Uses `ret_without_perf!`; belongs in portable validation |
| `tests_time`, the 29 cases that enable `max_timeslice` | `detcore/tests/time/mod.rs` | PMU (RCB counters); `tod_gettimeofday_faulting_tz_pkey_write_disabled_leaves_tv_unchanged` also needs PKU | Not ignored. Local `full` validation runs them in `privileged-test.pmu_detcore_time_cases`, after `privileged-pmu.preemption`; hosted CI does not run them. The two that disable `max_timeslice` run in `test.detcore_time` and its hosted twin (https://github.com/rrnewton/hermit/issues/3663) |
| `tests_parallelism` `futex_wait_parent`, `mem_race::`, `mem_print_race::` (`--ignored`) | `detcore/tests/parallelism*` | PMU (RCB counters) | |
| chaos schedule-bisection tests (`--ignored`) | `hermit-cli/tests/analyze.rs` | PMU **and** mount/user namespaces | `#[ignore]`: "requires PMU branch counters and working mount namespaces" |
| `strict_mode_matrix` PMU case (`--ignored`) | `hermit-cli/tests/hermit_modes.rs` | PMU and namespaces | |
| PMU-dependent slow stress tier (`--ignored`) | `hermit-cli/tests/stress_suite.rs` | PMU and namespaces | Also fast/slow stress tiers gated `#[ignore]` |
| `*_determinism`, `arbitrary_binaries`, `record_replay_matrix` | `hermit-cli/tests/` | User/mount namespaces (PMU for scheduling fidelity) | |
| language-runtime determinism | `hermit-cli/tests/language_runtime_determinism.rs` | Optional toolchains (Go, Ruby, Node.js, OpenJDK, OCaml, CPython) | `#[ignore]` per missing toolchain |
| `python_stdlib` | `hermit-cli/tests/python_stdlib.rs` | System CPython 3 + full `Lib/test` | |
| `redis_strict`, `sqlite_veryquick` | `hermit-cli/tests/` | Network/build to fetch+build pinned Redis/SQLite | Slow; `#[ignore]` by default |

`#[ignore]` tests are excluded from a plain `cargo test`. Scheduled or explicit
local validation may opt into them; running them requires the matching
capability, not just removing `--ignored`.

### KVM memory-hash repeatability: why the guest must be statically linked

`kvm_memory_hashes_repeat_for_a_static_guest`
(`hermit-cli/tests/kvm_info_log_determinism.rs`) asserts that KVM produces
identical stack and heap **content** hashes across two runs of the same guest. It
uses a **statically linked** guest deliberately, and that restriction is the
whole scope of the claim.

**A dynamically linked guest fails this property today, and not marginally.**
Measured on **devbig030**:

| guest | backend | stack-content hashes differing run to run |
| --- | --- | --- |
| `/bin/echo hello` (dynamic) | KVM | **98 of 113** |
| `/bin/echo hello` (dynamic) | ptrace | **0 of 193** |

The cause is known and tracked rather than papered over: the KVM backend never
delivers `rdtsc` to the Reverie `Tool`, so Detcore's existing virtualization never
runs and the guest reads a raw host-derived cycle counter, which the dynamic
loader then leaves on the stack. See
<https://github.com/rrnewton/reverie/issues/448>.

A static binary executes **zero** `rdtsc` (measured: 0, against 10 for every
dynamically linked guest tested), which is exactly why the property holds there
and only there.

When #448 lands, the static restriction should be removed and that test should
pass for a dynamic guest too — that is the intended signal.

> **Why this measurement lives here and not beside the test.** The figures above
> are only meaningful with the host they were taken on, and this project's
> reporting standard requires naming it. `scripts/check-portable-paths.sh`
> forbids literal hostnames in `.rs`, `.sh`, `.py` and similar build/run files —
> correctly, because a hostname in code is how a real host dependency starts —
> but it does not scan `.md`. Recording the provenance here keeps the measurement
> auditable without weakening that gate or writing a hostname into a test file.
> Renaming the host to something generic was rejected: it would leave a sentence
> that reads as evidence and carries none.

## Expected failure signatures

Match observed output to a cause before filing a bug. Exact strings live in
[docs/ERROR_CATALOG.md](ERROR_CATALOG.md).

### Missing or blocked PMU / perf permissions

- `--max-timeslice requires user-space perf counters ... continuing with timer preemption disabled`
- `perf_event_open is unavailable; continuing with --max-timeslice=disabled. Check the host perf_event_paranoid value ...` (`hermit-cli/src/bin/hermit/run.rs`)
- `Hardware perf counters are not supported on this machine. Records/Replays may randomly fail`
- Guest **hangs after a PMU warning**: timer preemption is disabled and a
  CPU-bound thread reaches no scheduling event.

**Action:** lower `/proc/sys/kernel/perf_event_paranoid`, grant PMU access, or
accept `--max-timeslice=disabled` (weaker scheduling fidelity). This is an
**environment** condition, not a Hermit bug.

### Unsupported / unrecognized CPU model

- `prehook: PMU RCB overshoot! ...` at ERROR means a precise PMU timer trapped
  after its expected RCB target. Detcore preserves the timer state and continues
  through normal timer handling. Add `--panic-on-rbc-overshoot` (also accepted
  as `--panic-on-rcb-overshoot`) to stop at the detection point for debugging.
- Startup timer/perf invariant errors such as `Couldn't read clock`,
  `end_of_timeslice is None`, `Timer invariant broken`, or `Failed to set timer`,
  on a host that *does* have a PMU.

**Action:** capture `lscpu` and the exact message. A PMU-capable bare-metal host
that repeatedly overshoots or rejects the model is a **CPU-support bug** worth
filing (include the diagnostic block below).

### CPUID interception / RDRAND/RDSEED mismatch

- `rdrand_rdseed_is_masked` fails an assertion like
  `virtual CPU should expose basic feature information`, or the post-mask
  `assert!(!feature.has_rdrand())` fails — the environment prevented CPUID
  faulting from taking effect or exposed an unexpected feature combination.
- `cpuid leaf 0x... subleaf 0x... not in deterministic table; returning zero
  result` — a guest probed a CPUID leaf with no deterministic table entry; the
  reported subleaf is the guest's ECX input.

**Action:** on a VM this is usually an **environment** limitation (no CPUID
faulting). On bare metal with faulting support, a reproducible mismatch may be a
product bug — report it with `grep -m1 '^flags' /proc/cpuinfo`. Do **not** weaken
the assertion to make a VM green.

### Missing namespaces

- Hermit integration tests fail to construct the container, or the CI
  mount-namespace probe reports unavailable and the job runs unit tests only.

**Action:** enable user/mount namespaces, or run on a host/runner that permits
them. Container runtimes frequently block these independently of CPU/PMU.

## Standard diagnostic block

Collect this before reporting any environment-related failure. It captures the
capability axes without leaking unrelated host detail.

```bash
uname -a
lscpu
grep -m1 '^flags' /proc/cpuinfo
cat /proc/sys/kernel/perf_event_paranoid
systemd-detect-virt || true
cargo test --workspace --no-fail-fast
cargo test -p hermit-detcore --test tests_misc -- --nocapture
```

What matters in the output:

- `uname -a` / `systemd-detect-virt`: kernel version and whether you are on bare
  metal, a VM, a container, or WSL.
- `lscpu`: CPU vendor/model — the key input for CPU-model support.
- `/proc/cpuinfo` flags: presence of `rdrand`/`rdseed` and related features.
- `perf_event_paranoid`: whether unprivileged `perf_event_open` is permitted.
- The two `cargo test` lines: which specific tests pass, fail, or skip.

**Redaction:** `lscpu`, `uname -a`, and `/proc/cpuinfo` can include hostnames,
serial numbers, microcode revisions, or internal identifiers. Post only the
CPU model, feature flags, kernel version, and virtualization type relevant to
the failure; remove hostnames and any internal identifiers before sharing.

## Troubleshooting flow

1. **Reproduce** with the diagnostic block above.
2. **Classify** the failure using the signatures:
   - PMU/perf or namespace signature → **adjust the environment** (grant perf
     access, enable namespaces, or use a privileged/bare-metal runner). Not a
     bug.
   - VM/container without PMU or CPUID faulting → **expected limitation**. Run
     the environment-independent subset only, or move to a capable host.
   - PMU-capable **bare-metal** host that still rejects the CPU model, or a
     reproducible CPUID/RDRAND mismatch **with** faulting support → likely a
     **product bug**; file it.
3. **Do not** delete or weaken a hardware-sensitive test to make a VM or
   restricted container green. Document the host-dependent skip instead.

## Bug-report checklist

When filing an environment-related bug, include:

- [ ] Output of the [standard diagnostic block](#standard-diagnostic-block)
      (redacted).
- [ ] The exact failing `hermit` command or `cargo test` invocation.
- [ ] The full error text and any preceding PMU/CPU/namespace warnings.
- [ ] Hermit revision (`git rev-parse HEAD`) and toolchain
      (`rustc --version`).
- [ ] Whether the failure reproduces on a second run and on a different host.
- [ ] Your classification from the [troubleshooting flow](#troubleshooting-flow)
      (environment vs suspected product bug), with reasoning.

## Related work and cross-links

- **Cargo integration-test port:** the public Cargo build does not yet cover
  Meta's 700+ internal Buck integration tests (see `AGENTS.md` → *Test*). A
  green `cargo test --workspace` is not full coverage.
- **Environment-related open issues** (current `rrnewton/hermit` tracker):
  - [#21](https://github.com/rrnewton/hermit/issues/21) — chaos stress wrapper
    falsely skips PMU-capable hosts
  - [#14](https://github.com/rrnewton/hermit/issues/14) — PMU parallelism tests
    emit unfiltered per-instruction timer traces
  - [#9](https://github.com/rrnewton/hermit/issues/9) — `vng` cannot discover the
    host kernel because Hermit virtualizes `uname -r`
  - [#6](https://github.com/rrnewton/hermit/issues/6) — virtualized host time
    corrupts QEMU guest clock calibration
  - [#94](https://github.com/rrnewton/hermit/issues/94) — privileged CI stays
    red after mount fix (statfs replay)
- **This issue:** [#11](https://github.com/rrnewton/hermit/issues/11).
- [docs/ERROR_CATALOG.md](ERROR_CATALOG.md) — exact error text → cause → fix.
- [docs/USER_GUIDE.md](USER_GUIDE.md) — host setup, PMU access, and
  troubleshooting for end users.
- [README.md](../README.md) — supported environment and quick troubleshooting.

> **Note on issue references:** issue #11's original text cited `#24`, `#40`,
> `#47`, and `#28` as bare-metal CPU failures and the Cargo-port tracker. In the
> current `rrnewton/hermit` tracker those numbers map to unrelated issues, so the
> concrete cross-links above point to the environment issues that actually exist
> today rather than to the stale numbers.
