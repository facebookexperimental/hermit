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
> test.** A green `cargo test --workspace` on a hosted VM demonstrates the
> environment-independent subset only, not the PMU- and namespace-dependent
> integration matrix.

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
| **Hosted VM (typical cloud)** | ✅ | varies | ❌ usually | n/a | ❌ often | often ✅ | Env-independent subset passes; PMU and CPUID/RDRAND tests fail or skip |
| **Self-hosted VM with virtualized PMU** | ✅ | ✅ | ✅ (if configured) | ✅ | varies | ✅ | Approaches bare metal; validate PMU tests explicitly before trusting them |
| **Container (shares host CPU)** | ✅ | inherits host | inherits host, but | ❌ often restricted | inherits host | ❌ often blocked | CPU/PMU capabilities come from the host, but perf perms and namespaces are commonly restricted independently |
| **WSL** | ✅ | varies | ❌ usually | n/a | varies | varies | Treat like a hosted VM; PMU-dependent tests are not expected to pass |

Notes:

- A container **shares the host's physical CPU and PMU**, but the runtime and
  kernel config can still block `perf_event_open` and namespace creation, so a
  container on a PMU-capable host is not automatically PMU-capable for tests.
- "Skip" vs "fail": some hardware-sensitive tests now guard their prerequisites
  and print a skip message instead of failing (see
  [Hardware-sensitive tests](#hardware-sensitive-cargo-tests)). Older reports of
  hard failures on VMs may predate those guards.

## CI tiers: what runs where

CI (`.github/workflows/ci.yml`) is the reference for which tests are expected in
ordinary GitHub Actions versus a specialized runner. There are two jobs:

### `regular` — GitHub-hosted (`ubuntu-latest`)

Runs on every push and pull request. Covers the **environment-independent
subset**:

- `cargo build --workspace`
- `cargo nextest run --profile ci --workspace` **excluding** `detcore`,
  `hermit`, and `hermetic_infra_hermit_flaky-tests`
- `cargo test -p hermit --lib --bins` (no namespace-dependent integration tests)
- `cargo test -p detcore --lib --bins` and
  `cargo test -p detcore --test tests_misc getrandom_intercepted -- --exact`
  (PMU-free: this test calls `reverie_ptrace::ret_without_perf!()`)
- doc tests (`cargo test --workspace --doc`), `cargo doc`, Clippy, rustfmt

GitHub-hosted runners have **no usable PMU and no CPUID faulting**, so the
detcore and hermit integration suites are deliberately excluded here.

### `hardware` — self-hosted (`[self-hosted, Linux, X64, hermit, pmu]`)

Runs on push, and on pull requests only from the trusted `rrnewton` account.
Requires a bare-metal-class host with PMU access. Covers:

- **CPUID/RDRAND/RDSEED:** `tests_misc has_rdrand_without_detcore`,
  `tests_misc rdrand_rdseed_is_masked`
- **PMU timing/parallelism:** `tests_time --ignored`,
  `tests_parallelism futex_wait_parent --ignored`,
  `tests_parallelism 'mem_race::' --ignored`,
  `tests_parallelism 'mem_print_race::' --ignored`
- **Namespace-gated Hermit integration** (only if a mount-namespace probe
  succeeds): `arbitrary_binaries`, `cli`, `clock_determinism`,
  `epoll_determinism`, `mmap_determinism`, `procfs_determinism`,
  `signal_determinism`, `record_replay_matrix`, `strict_mode_matrix`, the
  fail-closed ratchet (`scripts/test-fail-closed.sh`), the working-envelope gate
  (`validate.sh --envelope-compare`), and the debugger integration tests
- **Backend parity ratchet:** always for `ptrace`; `kvm` only when `/dev/kvm` is
  readable+writable; `dbi` only when the DynamoRIO environment is configured

If the mount-namespace probe fails, the job falls back to
`cargo test -p hermit --lib --bins` only.

## Hardware-sensitive Cargo tests

Named tests and the capabilities they require. Paths are relative to the repo
root.

| Test / group | File | Requires | Notes |
| --- | --- | --- | --- |
| `has_rdrand_without_detcore` | `detcore/tests/misc/mod.rs` | Host RDRAND | Probes host features; returns early if RDRAND absent |
| `rdrand_rdseed_is_masked` | `detcore/tests/misc/mod.rs` | RDRAND/RDSEED **and** CPUID faulting | Runs without PMU (`det_test_fn_without_pmu`); skips if faulting unsupported |
| `getrandom_intercepted` | `detcore/tests/misc/mod.rs` | None (PMU-free) | Uses `ret_without_perf!`; runs on GitHub-hosted CI |
| `tests_time` (`--ignored`) | `detcore/tests/time.rs` | PMU (RCB counters) | |
| `tests_parallelism` `futex_wait_parent`, `mem_race::`, `mem_print_race::` (`--ignored`) | `detcore/tests/parallelism*` | PMU (RCB counters) | |
| chaos schedule-bisection tests (`--ignored`) | `hermit-cli/tests/analyze.rs` | PMU **and** mount/user namespaces | `#[ignore]`: "requires PMU branch counters and working mount namespaces" |
| `strict_mode_matrix` PMU case (`--ignored`) | `hermit-cli/tests/hermit_modes.rs` | PMU and namespaces | |
| PMU-dependent slow stress tier (`--ignored`) | `hermit-cli/tests/stress_suite.rs` | PMU and namespaces | Also fast/slow stress tiers gated `#[ignore]` |
| `*_determinism`, `arbitrary_binaries`, `record_replay_matrix` | `hermit-cli/tests/` | User/mount namespaces (PMU for scheduling fidelity) | |
| language-runtime determinism | `hermit-cli/tests/language_runtime_determinism.rs` | Optional toolchains (Go, Ruby, Node.js, OpenJDK, OCaml, CPython) | `#[ignore]` per missing toolchain |
| `python_stdlib` | `hermit-cli/tests/python_stdlib.rs` | System CPython 3 + full `Lib/test` | |
| `redis_strict`, `sqlite_veryquick` | `hermit-cli/tests/` | Network/build to fetch+build pinned Redis/SQLite | Slow; `#[ignore]` by default |

`#[ignore]` tests are excluded from a plain `cargo test`; the `hardware` CI job
opts into them with `-- --ignored`. Running them locally requires the matching
capability, not just removing `--ignored`.

## Expected failure signatures

Match observed output to a cause before filing a bug. Exact strings live in
[docs/ERROR_CATALOG.md](ERROR_CATALOG.md).

### Missing or blocked PMU / perf permissions

- `--preemption-timeout requires user-space perf counters ... continuing with timer preemption disabled`
- `perf_event_open is unavailable; continuing with --preemption-timeout=disabled. Check the host perf_event_paranoid value ...` (`hermit-cli/src/bin/hermit/run.rs`)
- `Hardware perf counters are not supported on this machine. Records/Replays may randomly fail`
- Guest **hangs after a PMU warning**: timer preemption is disabled and a
  CPU-bound thread reaches no scheduling event.

**Action:** lower `/proc/sys/kernel/perf_event_paranoid`, grant PMU access, or
accept `--preemption-timeout=disabled` (weaker scheduling fidelity). This is an
**environment** condition, not a Hermit bug.

### Unsupported / unrecognized CPU model

- Startup timer/perf invariant errors such as `Couldn't read clock`,
  `Missed expected preemption`, `end_of_timeslice is None`,
  `Timer invariant broken`, or `Failed to set timer`, on a host that *does* have
  a PMU.

**Action:** capture `lscpu` and the exact message. A PMU-capable bare-metal host
that still rejects the model is a **CPU-support bug** worth filing (include the
diagnostic block below).

### CPUID interception / RDRAND/RDSEED mismatch

- `rdrand_rdseed_is_masked` fails an assertion like
  `virtual CPU should expose basic feature information`, or the post-mask
  `assert!(!feature.has_rdrand())` fails — the environment prevented CPUID
  faulting from taking effect or exposed an unexpected feature combination.
- `cpuid leaf 0x... not in deterministic table; returning zero result` — a guest
  probed a CPUID leaf with no deterministic table entry.

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
cargo test -p detcore --test tests_misc -- --nocapture
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
     access, enable namespaces, or use a self-hosted/bare-metal runner). Not a
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
  - [#94](https://github.com/rrnewton/hermit/issues/94) — self-hosted CI stays
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
