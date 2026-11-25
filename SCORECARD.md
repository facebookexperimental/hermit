# Test selection scorecard

Hermit runs Linux programs reproducibly by controlling thread scheduling, clocks, and randomness. This scorecard shows which test combinations the full regression suite selects; the [compatibility tracker](https://hermetic-infra.org/compatibility/latest/) shows recorded results.

A **backend** is how Hermit observes and controls a running program. The default, best-tested backend uses Linux's `ptrace` process-tracing interface and serves as the reference for comparisons. Backends differ in execution overhead, host requirements, and program support.

The suite is a matrix of **(program, mode, backend)** combinations, called cells. `verify` compares repeated runs; `replay` checks a recording against its replay; `chaos` checks repeatability for each selected scheduling seed. `naked` runs a program directly on Linux, labelled `native` as the baseline. One program can contribute several cells.

The headline counts sum **all modes** for each backend. For ptrace, **1195** selected cells comprise **615 verify**, **573 replay**, **7 chaos**. They count selected test combinations, not unique programs or passing results. A test passes when it completes as expected and satisfies its mode's checks; see [Methods](#methods).

## Selection by backend

**Selected by full** means included in the full regression suite. **Not selected by full** means an applicable combination outside that suite. **Not applicable** means the test configuration does not enable that program, mode, and backend combination. Selection is not a test result: an unselected cell may have passed, failed, or never run.

Of **17822** total cells, **2436** are selected by full, **1846** are not selected by full, and **13540** are **Not applicable**.

| Backend | Selected by full | Not selected by full | Not applicable | Total cells |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 1195 | 359 | 1260 | 2814 |
| `dbt` | 0 | 561 | 2253 | 2814 |
| `kvm` | 610 | 141 | 2063 | 2814 |
| `sabre` | 0 | 480 | 2334 | 2814 |
| `liteinst` | 316 | 136 | 2362 | 2814 |
| `in-guest-trap` | 315 | 136 | 2363 | 2814 |
| `native` | 0 | 33 | 905 | 938 |
| **Total** | **2436** | **1846** | **13540** | **17822** |

`dbt` (dynamic binary translation) retains the test catalogue for the [separately developed DynamoRIO implementation](https://github.com/rrnewton/hermit-dynamorio): **561** applicable cells, **0** selected by full. This is not a current Hermit command-line backend choice.

**Archived:** `sabre` (backend, archived 2026-10-25, test schedule: nightly only) and `e9patch` (binary preprocessing with ptrace, archived 2026-10-25, test schedule: nightly only). These receive no feature development. The listed schedules describe archive policy; the compatibility tracker shows recorded results. The catalogue retains **480** applicable cells for `sabre`; **0** are selected by full and **480** are outside that suite.

## Selection by mode

Each backend entry is `selected by full / total cells`, including not-applicable combinations in the total. An em dash means that mode is not defined for that backend or the native baseline.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `in-guest-trap` | `native` | Selected by full | Not selected by full | Not applicable | Total cells |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 615 / 938 | 0 / 938 | 610 / 938 | 0 / 938 | 316 / 938 | 315 / 938 | — | 1856 | 1776 | 1996 | 5628 |
| `replay` | 573 / 938 | 0 / 938 | 0 / 938 | 0 / 938 | 0 / 938 | 0 / 938 | — | 573 | 37 | 5018 | 5628 |
| `chaos` | 7 / 938 | 0 / 938 | 0 / 938 | 0 / 938 | 0 / 938 | 0 / 938 | — | 7 | 0 | 5621 | 5628 |
| `naked` | — | — | — | — | — | — | 0 / 938 | 0 | 33 | 905 | 938 |
| **Total** | | | | | | | | **2436** | **1846** | **13540** | **17822** |

## Ptrace workloads

The reference backend's tests span small syscall exercises, applications, and language runtimes. Each mode entry is `selected by full / total cells`, with the same counts as above.

| Test category | Verify | Replay | Chaos | Selected by full | Total cells |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 7 / 7 | 7 / 7 | 0 / 7 | 14 | 21 |
| `bin-c` | 2 / 2 | 1 / 2 | 0 / 2 | 3 | 6 |
| `c-programs` | 304 / 306 | 266 / 306 | 3 / 306 | 573 | 918 |
| `chaos-c` | 1 / 1 | 1 / 1 | 1 / 1 | 3 | 3 |
| `compat` | 212 / 221 | 215 / 221 | 0 / 221 | 427 | 663 |
| `data-handling` | 6 / 6 | 6 / 6 | 0 / 6 | 12 | 18 |
| `debugger-c` | 1 / 1 | 1 / 1 | 0 / 1 | 2 | 3 |
| `determinism-stress` | 6 / 6 | 6 / 6 | 2 / 6 | 14 | 18 |
| `determinism-stress-c` | 11 / 11 | 11 / 11 | 1 / 11 | 23 | 33 |
| `gvisor` | 0 / 277 | 0 / 277 | 0 / 277 | 0 | 831 |
| `language-runtimes` | 22 / 22 | 19 / 22 | 0 / 22 | 41 | 66 |
| `rr` | 0 / 32 | 0 / 32 | 0 / 32 | 0 | 96 |
| `shared-futex-c` | 1 / 4 | 0 / 4 | 0 / 4 | 1 | 12 |
| `system-utils` | 41 / 41 | 40 / 41 | 0 / 41 | 81 | 123 |
| `util-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |

The full compatibility suite runs 2441 checks: the 2436 selected matrix cells above (including 7 chaos tests), plus 5 additional commands listed below. All must produce a fresh passing result for the validation run to succeed.

### Additional regression tests

These tests use custom commands instead of the common modes. They run in the full regression suite but are outside the matrix and its counts.

| Lane | Category | Test | Mode | Backend |
| --- | --- | --- | --- | --- |
| `portable` | `c-programs` | `c-programs/environment-and-workdir` | `custom` | `ptrace` |
| `portable` | `c-programs` | `c-programs/inotify-protection-key-fault` | `custom` | `ptrace` |
| `portable` | `c-programs` | `c-programs/io-uring-fallback` | `custom` | `ptrace` |
| `portable` | `c-programs` | `c-programs/random-device-spellings` | `custom` | `ptrace` |
| `portable` | `system-utils` | `system-utils/clock-determinism` | `custom` | `ptrace` |

## Known gaps

These **2** tests document unresolved backend limitations in additional test suites, outside the matrix and every count above. They are either run with an expected failure or skipped, as shown below; neither status counts as a pass. A skipped test has no current result. The linked issues describe the missing behavior.

### `privileged-test.deadline_slice_cap_kvm` (`--backend=kvm`)

**43** required tests must pass; **2** known gaps (2 skipped, not run), from `hermit-cli/tests/deadline_slice_cap.kvm-gaps.tsv`:

| Test | Gap | Tracking issue | Must fail with | Status |
| --- | --- | --- | --- | --- |
| `itimer_a_private_descriptor_table_consumer_re_arms` | D — threads with private file descriptor tables | https://github.com/rrnewton/hermit/issues/4485 | not run | skipped (known gap) |
| `itimer_generation_discard_follows_the_wait_mask` | E&#43;F&#39; — interval&#45;timer expiry during waits with temporary signal masks | https://github.com/rrnewton/hermit/issues/4995 | not run | skipped (known gap) |

## Methods

### What the counts cover

The matrix retains these backend names and modes; catalogue membership does not imply support in the current Hermit command:

- Backend catalogue: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `in-guest-trap`
- Baseline: `native`
- Modes: `chaos`, `naked`, `replay`, `verify`

The full regression suite selects **2436 / 17822** cells (**13.67%**). Of the total, **13540** are not applicable. Excluding those gives **2436 / 4282** applicable cells (**56.89%**). These describe the same selection with different denominators; neither is a pass rate. Adding programs, backends, or modes changes the denominator, so comparisons over time must use the same population.

### What the tests check

Full `verify` comparisons run the same backend twice and compare exit status, standard output, standard error, and nonempty INFO event logs. Log comparison removes host wall-clock prefixes and consistently renames host addresses while preserving their identity and order; virtual time, branch counts, syscall values, and other event details must match. All **1856** selected `verify` cells require this full comparison.

`replay` checks a recording against its replay. `chaos` checks repeatability for each selected scheduling seed. Repeated runs on one backend do not establish agreement with another backend; the tracker reports [cross-backend agreement](https://hermetic-infra.org/compatibility/latest/#parity) separately.

See the [test-suite documentation](ci/compat-envelope/README.md) for the detailed rules.

## Test results over time

The [compatibility tracker](https://hermetic-infra.org/compatibility/latest/) shows recorded outcomes and trends. Its source results are available in [hermit_test_ledger](https://github.com/rrnewton/hermit_test_ledger).
