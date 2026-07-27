<!--
Copyright (c) Meta Platforms, Inc. and affiliates.
All rights reserved.

This source code is licensed under the BSD-style license found in the
LICENSE file in the root directory of this source tree.
-->

# CI and validate.sh alignment

The Rust workflow and local validation use the same capability selectors:

| Lane | Runner | Command | Capability contract |
| --- | --- | --- | --- |
| Portable | GitHub-hosted Ubuntu | `./validate.sh --hosted-only --no-label-pr` | No PMU counters, ptrace CPUID faulting, or KVM required |
| Hardware | self-hosted `[Linux, X64, hermit, pmu, pmu-serial]` | `./validate.sh --hardware-only --no-label-pr` | PMU, CPUID, KVM, or another host capability is part of the test |

The default `./validate.sh` remains the developer superset. The focused modes
exist so either CI subset can be reproduced independently. The tiered workflow
maps `quick` to portable, `full` to per-PR hardware, and long database and
PMU chaos stress to the weekly self-hosted `super` profile.

## Portable lane

The hosted job enables user and mount namespaces before starting Hermit. This
is a privilege prerequisite, not a PMU or CPUID dependency. Guest tests in this
lane either need no hardware event handling or explicitly pass:

```text
--max-timeslice=disabled --no-virtualize-cpuid
```

The selector covers exactly 476 of the 888 Cargo-discovered cases:

| Group | Cases | Selector |
| --- | ---: | --- |
| Workspace unit, bin, and doc baseline | 329 | Existing regular-job selection |
| Detcore misc without CPUID probes | 22 | `tests_misc`, excluding two RDRAND/CPUID cases and three bounded diagnostics |
| Detcore parallel without RCB scheduling | 5 | Raw/noop cases, excluding generated `detcore` variants |
| Flaky guest crate contract | 1 | The crate's standalone Cargo test |
| Portable Hermit integration cases | 119 | Non-KVM CLI, non-python3-verify LiteInst, strict/verify modes, clock-discipline, futex2, pidfd creation, and process-isolation refusals, non-JVM apps, commands, time, memory, procfs, signals, Python, and rr source contract |

The same lane enforces the 12 portable L1-L4 working-envelope cells and runs
the 181-row strict compatibility corpus with the debug Hermit binary and PMU/CPUID disabled.
The corpus is blocking except for seven bounded diagnostics observed on a
GitHub-hosted runner with PMU disabled: Rust, Java/Javac, Node.js, the two zstd
rows, and `top` process-table variance.

The lane also requires all six DynamoRIO DBI parity scenarios currently
marked `pass`. Cargo builds the pinned DynamoRIO runtime and native client;
external `DYNAMORIO_HOME`, `HERMIT_DRRUN`, and `HERMIT_DBI_CLIENT` variables are
not part of the CI contract.

## Hardware lane

The remaining 412 Cargo cases are outside the blocking hosted subset. The
per-PR hardware lane executes 319 blocking cases, 18 cases run as bounded
nonblocking diagnostics (eleven hosted and seven hardware), the weekly `super`
tier executes 69 long or relaxed cases, and five existing gaps remain explicit:

| Group | Cases | Routing | Hardware reason |
| --- | ---: | --- | --- |
| Detcore CPUID/RDRAND probes | 2 | Per-PR | Host feature probe and deterministic masking |
| Detcore time tests | 14 | Per-PR | Nonzero RCB preemption configuration |
| Detcore parallel variants | 11 | Per-PR | Deterministic RCB preemption assertions |
| KVM CLI cases | 17 | Per-PR | Read/write `/dev/kvm` is required |
| DBI pipe backpressure | 1 | Per-PR diagnostic | Bounded known DBI hang from PR #598 |
| Portable runtime diagnostics | 6 | Hosted per-PR diagnostic | Threaded Java/Node matrix, four python3 `--verify` LiteInst shape-ordering cases, and chaos hello-race gaps |
| Portable no-PMU diagnostics | 5 | Hosted per-PR diagnostic | Post-fork, network, IPC, and random-source stalls |
| Pselect signal interruption | 1 | Per-PR diagnostic | Virtual timeout currently wins the delayed-signal race |
| Buck chaos variants | 8 | Weekly | Explicit one-million-RCB time slice |
| Relaxed default-mode cases | 55 | 53 weekly, 2 known ignored gaps | Non-sequentialized relaxed execution can block without hardware scheduling |
| Portable chaos/stress cases | 5 | Weekly | Seed searches exceed hosted per-gate budgets |
| Runtime, database, scheduling, and syscall targets | 53 | 52 per-PR blocking, 1 per-PR diagnostic | Default PMU/CPUID or record/replay configuration; includes the relocated five-run thread-sync determinism target |
| Ignored runtime/database/analyze tiers | 19 | 12 per-PR, 3 weekly, 4 JVM diagnostics | Default PMU/CPUID configuration |
| Slow CAS stress | 1 | Per-PR | PMU preemption search and replay |
| rr syscall corpus | 213 | 210 per-PR, 3 known gaps | Explicit 80-million-RCB time slice |

The Detcore time and parallel commands intentionally do not pass `--ignored`.
Those targets contain no ignored tests; the former workflow selected zero
cases. Each of the 25 PMU cases runs as an exact, single-threaded Cargo test so
a leaked tracee cannot hold a whole family harness open. Timing cases have a
two-minute limit, futex cases five minutes, and the CPU-heavy memory cases 15
minutes each. A family stops after its first failure, while a green run still
executes every enumerated case.

The per-PR hardware lane runs LevelDB's bounded `env_posix_test`. The eight
Buck chaos cases, PMU-skid-sensitive `analyze_hello_race`, full randomized
LevelDB suite, SQLite veryquick suite, 53 relaxed default-mode cases, and five
portable chaos/stress cases remain in the weekly `super` profile because they
cannot fit the per-PR hosted budget. The two intentionally ignored default-mode
known hangs remain explicit gaps.

The rr gate excludes `rr_ppoll` (unsupported `ppoll` operation), `rr_rlimit`
(host policy rejects `setrlimit`), and `rr_sched_yield_to_lower_priority` (priority scheduling gap).

The stable record/replay integration cases remain blocking. The intermittently
flaky `record_replay_matrix`, four JVM cases, the DBI pipe-backpressure case,
the pselect signal-interruption case, and the eleven hosted diagnostics
still execute on every pull request as bounded nonblocking diagnostics tracked
by PRs #678, #657, #598, #673, and #712.

The hardware lane also gates the three record/replay working-envelope cells,
the 128-row R/R compatibility corpus, debugger
integration, and ptrace backend parity. Missing rr sources, namespaces, PMU,
CPUID, KVM, or runtime prerequisites fail the lane instead of silently reducing
coverage.

## Workflow prerequisites

The hosted job installs the Unix commands, language tools, app binaries,
cargo-nextest, rustfmt, and Clippy used by its selected tests. The self-hosted
job initializes the rr submodule and preflights PMU, KVM, namespaces, runtime
tools, databases, and debuggers before starting the hardware selector. Hardware
and weekly-super jobs require the dedicated `pmu-serial` runner label so PMU
tests cannot overlap unrelated work on the two general self-hosted runners.

When adding a test:

1. Identify whether it asserts PMU/RCB, CPUID, KVM, or another host capability.
2. Put it in exactly one validation tier.
3. If it is portable, make the disabling flags explicit in its command helper.
4. Run both focused modes on a capable host and the default full validation.
5. Update the case totals in this document.
