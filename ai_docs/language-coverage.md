# Language and Runtime Coverage

Audit date: 2026-07-22

Host: CentOS Stream 9, x86_64, Linux
`6.13.2-0_fbk13_hardened_0_g02230262e956`.

This document distinguishes active Cargo coverage, opt-in coverage, legacy
Buck/lit sources, open pull requests, and untracked experiments. A source file
in the repository is not automatically a test that public Cargo CI executes.

## Summary

The audited test and benchmark roots contain the following source-language
inventory after this audit:

| Language | Source files | Effective coverage |
| --- | ---: | --- |
| Rust | 79 | Primary implementation, unit, integration, and guest-test language. |
| C | 40 | Broad syscall guests, deterministic scenarios, chaos tests, and benchmark fixtures. |
| C++ | 3 | Flaky guests only on main; larger OSS workloads are open PRs. |
| Python | 10 | Harnesses/utilities plus one opt-in OSS CPython entropy test. |
| Shell | 29 | Test and experiment orchestration, not a compiler-runtime claim. |
| Go | 2 | One legacy lit hello-world guest and one opt-in entropy test. |
| Ruby | 1 | New opt-in entropy test. |
| Java | 1 | New opt-in OpenJDK entropy test. |
| JavaScript | 1 | New opt-in Node.js entropy test. |
| OCaml | 1 | New opt-in native-code entropy test. |

The counts cover tracked test, benchmark, example, and script roots plus the
new files in this audit. They exclude the imported `third-party/rr` source
tree and generated artifacts under `target/`.

### Coverage status

- Rust and C have substantial active Cargo coverage.
- Go has a legacy Buck/lit test, but that matrix is not fully ported to public
  Cargo.
- The merged arbitrary-binary test only runs version/smoke commands for
  Python, Node.js, Java, and Go when those binaries happen to be installed.
  Absence is optional and does not prove determinism.
- This audit adds explicit entropy-removal tests for Go, Ruby, Node.js,
  OpenJDK, OCaml, and OSS CPython. They are opt-in because they require external
  toolchains; invoking the matrix fails loudly when a tool is absent.
- Several larger C/C++/Python workloads exist only in open PRs. They must not be
  reported as main-branch coverage.

## Audited Toolchains

These are the exact versions used for this audit. Except for Rust, most test
harnesses currently select a system tool and do not pin the compiler/runtime
version.

| Language/runtime | Audit binary | Version | Version policy |
| --- | --- | --- | --- |
| Rust | `rustc` | `1.99.0-nightly (be8e82435 2026-07-11)`, LLVM 22.1.8 | Pinned by `rust-toolchain.toml`. |
| Cargo | `cargo` | `1.99.0-nightly (59800466c 2026-07-07)` | Selected with the Rust toolchain. |
| C | `/usr/bin/gcc` | GCC 11.5.0, Red Hat 11.5.0-14 | Host-selected, not pinned in Cargo tests. |
| C++ | `/usr/bin/g++` | GCC 11.5.0, Red Hat 11.5.0-14 | Host-selected; some experiment metadata records it. |
| Go | `/usr/bin/go` | Go 1.26.4, Red Hat 1.26.4-1.el9 | Host-selected. |
| Ruby | `/usr/bin/ruby` | Ruby 3.0.7p220 | Host-selected. |
| Node.js | `/usr/bin/node` | Node.js 16.20.2, npm 8.19.4 | Host-selected. |
| JVM | `/usr/bin/java`, `/usr/bin/javac` | OpenJDK 26.0.1+8 | Host-selected. |
| OCaml | `/usr/bin/ocamlopt` | OCaml 4.11.1 | Host-selected. |
| Python | `/usr/bin/python3` | OSS CPython 3.9.25, GCC 11.5.0 | Explicit path required. |
| Meta Python | `/usr/local/bin/python3` | fbpython 3.12.13+meta | Rejected for OSS coverage. |
| Shell | `/usr/bin/bash` | GNU Bash 5.1.8 | Host-selected. |
| Tcl | `/usr/bin/tclsh` | Tcl 8.6.10 | Used by upstream Redis/SQLite suites in open PRs. |

Installed RPMs include `golang-1.26.4`, `ruby-3.0.7`, `nodejs-16.20.2`,
`java-latest-openjdk-devel-26.0.1`, `ocaml-4.11.1`, and
`python3-3.9.25`.

### Python interpreter finding

`python3` resolves first to `/usr/local/bin/python3`, which is a symlink to
Meta fbpython. The OSS interpreter is `/usr/bin/python3`, a symlink to
`/usr/bin/python3.9` owned by the CentOS `python3` RPM. Runtime tests must use
the latter explicitly and reject version strings containing `fbpython` or
`+meta`.

## Merged Test Inventory

### Rust

All entries use the repository nightly Rust toolchain above.

| Suite | Tests/targets | Cargo status |
| --- | --- | --- |
| Workspace unit/doc tests | `common/digest`, `common/edit-distance`, `common/test-allocator`, `detcore-model`, `detcore`, `hermit`, `hermit-verify` | Active. |
| Detcore integration | `tests_misc`, `tests_parallelism`, `tests_time` | Active; some cases are ignored for PMU/hardware requirements. |
| Hermit CLI integration | `arbitrary_binaries`, `cli`, `clock_determinism`, `epoll_determinism`, `hermit_modes`, `ipc_determinism`, `mmap_determinism`, `procfs_determinism`, `random_determinism`, `record_replay`, `signal_determinism`, `stress_suite`, `thread_sync_determinism` | Active on capable/self-hosted hosts. |
| Runtime matrix | `language_runtime_determinism` with six tests | New, explicit `--ignored` matrix because external toolchains are required. |
| Hermit Verify integration | `hermit-verify/tests/cli.rs` | Active. |
| Cargo guest package | 32 `[[bin]]` targets in `tests/Cargo.toml` | Built as guests; not each one is an end-to-end Cargo test. |
| Flaky guest package | `cas_sequence_easy_bin`, `hello_race`, `hello_race_mini` | Built separately and commonly excluded from normal workspace tests. |

The 32 Rust guest targets cover chaos scheduling, bind/connect races, clocks,
exit, futexes, heap/stack addresses, memory races, nanosleep, networking,
pipes, poll, RDTSC, scheduler yield, socketpairs, and random/thread behavior.

The legacy lit tree adds 20 Rust guest programs covering descriptor allocation,
close-on-exec, file races, fstat, open/openat, pipes, read errors, affinity,
utime/utimes, and hello/race scenarios. Those sources have 45 `.lit` run
specifications, but public Cargo does not currently execute the complete Buck
matrix.

### C

All locally compiled C guests use GCC 11.5.0 in this audit. The repository does
not pin GCC.

| Group | Inventory | Execution status |
| --- | --- | --- |
| `tests/c` | 31 guests | A broad subset is compiled by Hermit integration tests; the rest are legacy/manual guests. |
| Dedicated determinism guests | `clock_determinism`, `epoll_determinism`, `ipc_determinism`, `mmap_determinism`, `random_sources`, `signal_determinism`, `thread_sync_determinism` | Active Hermit integration coverage. |
| Mode/record-replay guests | `getpid`, `uname`, `sysinfo`, `wait_on_child`, `nanosleep-par`, plus 15 compatibility guests selected by `hermit_modes` | Active on capable/self-hosted hosts. |
| Minimal guests | `hello_nostdlib`, `nanosleep-threads-nocrash`, `nanosleep-threads-simple`, `racewrite_nostdlib` | Manual/analyze/stress use. |
| Legacy lit C | `hello_world_c`, `networking`, `rt_sigaction`, `rt_sigprocmask` | Buck/lit coverage; partially ported through `hermit_modes`. |
| Chaos C | `lock_granularity`, `order_violation` | Guest sources, not standalone Cargo integration targets. |
| Benchmark C | `thread_counter`, `fork_exec_chain` | Active in `benchmarks/run.py`. |
| Hardware utility | `pmu_skid.c` | Manual PMU diagnostic. |

The remaining `tests/c` guests cover clone/vfork, CPU identity, alarms,
signals, memory pressure/address reporting, thread exhaustion, timed signal
waits, resource metadata, and sysinfo uptime.

### C++

Main contains only three C++ flaky guests:

- `flaky-tests/bind/bind_random.cpp`;
- `flaky-tests/bind/bind_same.cpp`; and
- `flaky-tests/use_configurable_flaky_service.cpp`.

They use the host C++ compiler when built by legacy/manual flows. No current
Cargo manifest exposes them as default integration tests. LULESH, LevelDB, and
Ninja coverage is still in open PRs listed below.

### Python

Merged Python files are primarily harnesses and utilities:

- `benchmarks/run.py` requires Python 3.9+;
- `detcore/tests/lit/generate-test.py` generates lit fixtures;
- `tests/util/simplest_server.py` and `ssl_server.py` support network tests;
- three `tests/python/non_strict` fixtures and two examples exercise simple
  output, random data, and timed progress; and
- the arbitrary-binary matrix runs `/usr/bin/python3 -c` when present.

The new `tests/runtime/random.py` plus
`python_runtime_entropy_is_determinized` is the first explicit OSS CPython
native-nondeterministic/strict-deterministic oracle in this worktree.

### Go

`detcore/tests/lit/hello_world_go` has normal, strict, and strict-verify lit
specifications. It is legacy Buck/lit coverage and has no current Cargo test
target. Its Go compiler version was not pinned.

The new `go_runtime_entropy_is_determinized` test compiles
`tests/runtime/random.go` with Go 1.26.4 and exercises `crypto/rand`.

### Ruby

Ruby had no prior tracked test. The new
`ruby_runtime_entropy_is_determinized` test uses `Random.new_seed` under Ruby
3.0.7. The distro Ruby currently fails in its RubyGems/did_you_mean prelude
with an `RbConfig` `NameError`, so the test uses `--disable-gems`. That is a
host packaging issue, not a Hermit syscall failure.

### Node.js

The merged arbitrary-binary matrix runs `node -e` as an optional smoke test.
It does not assert removal of nondeterminism. The new
`node_runtime_entropy_is_determinized` test uses `crypto.randomBytes` under
Node.js 16.20.2.

An untracked shared-futex experiment also contains a four-worker
`SharedArrayBuffer`/`Atomics.wait` workload. It passed 3/3 on Node.js 16.20.2
against PR #53 commit `b44d939`, but that implementation is not an ancestor of
the audited branch and the experiment is not Cargo CI coverage.

### JVM

The merged arbitrary-binary matrix runs `java -version` as an optional smoke
test. The new `jvm_runtime_entropy_is_determinized` test compiles and runs
`SecureRandom` with OpenJDK 26.0.1, `-Xint`, and one active processor.

The untracked shared-futex experiment includes an eight-thread latch and
atomic-counter workload. It passed 3/3 with Temurin OpenJDK 8u492 against PR
#53 commit `b44d939`; it is not evidence for the current branch.

### OCaml

OCaml had no prior test. The new `ocaml_runtime_entropy_is_determinized` test
compiles `Random.self_init` with `ocamlopt` 4.11.1.

### Shell

The 29 shell files drive standalone, networking, replay, analyze, chaos,
service, and legacy lit scenarios. They use host Bash/sh and external tools.
They are orchestration coverage, not proof of a separate language runtime.

## Benchmark Inventory

The merged benchmark harness uses OSS CPython 3.9+ and contains five workloads:

| Benchmark | Implementation | Audit version |
| --- | --- | --- |
| `echo` | GNU coreutils executable | coreutils 8.32 |
| `sort_1m_lines` | GNU coreutils executable | coreutils 8.32 |
| `grep_large_file` | GNU grep executable | grep 3.9 |
| `multithread_counter` | C11/pthreads fixture | GCC 11.5.0 |
| `fork_exec_chain` | C11 fork/exec fixture | GCC 11.5.0 |

The harness compares native and deterministic Hermit timing. It does not
itself prove output determinism and its external utility versions are not
pinned.

## New Runtime Determinism Matrix

The matrix command is:

```bash
cargo test -p hermit --test language_runtime_determinism -- \
  --ignored --test-threads=1 --nocapture
```

Each test makes five native attempts, requires more than one output, then
requires one unique output across three explicit `hermit run --strict`
attempts. Manual audit probes used five strict attempts.

| Runtime | Entropy API | Native outputs | Strict outputs | Normal strict result | First fail-closed gap |
| --- | --- | ---: | ---: | --- | --- |
| Go 1.26.4 | `crypto/rand` | 5 unique | 1 unique | Pass | `gettid` |
| Ruby 3.0.7 | `Random.new_seed` | 5 unique | 1 unique | Pass | `pread64` |
| Node.js 16.20.2 | `crypto.randomBytes` | 5 unique | 1 unique | Pass | `pread64` |
| OpenJDK 26.0.1 | `SecureRandom` | 5 unique | 1 unique | Pass | `pread64` |
| OCaml 4.11.1 | `Random.self_init` | 5 unique | 1 unique | Pass | `pread64` |
| OSS CPython 3.9.25 | `os.urandom` | 5 unique | 1 unique | Pass | `pread64` |

`--panic-on-unsupported-syscalls` stops at the first listed syscall, so this
table is a lower bound rather than a complete syscall trace. Normal strict mode
currently passes those calls through. Stable entropy output proves the tested
random value is controlled; it does not prove all runtime behavior is fully
modeled.

The host also reports that CPUID faulting is unavailable during every Hermit
run. The entropy outputs still match, but broader CPUID-dependent behavior is
not established by this machine.

## Open and Experimental Workload Coverage

These results are useful evidence but are not merged main-branch coverage.

| PR/experiment | Language and version | Coverage/result | Remaining gap |
| --- | --- | --- | --- |
| #77 LULESH | C++ LULESH 2.0.3 at `46c2a1d`; GCC 11.5.0 + libgomp | Four-thread OpenMP result matched across 2 and 5 strict runs. | Open PR. |
| #83 LevelDB | C++ LevelDB at `7ee830d`; compiler selected by CMake | `c_test` plus 15 focused cases pass twice; heavy suite hits 15-minute guard. | Compiler version is not enforced; open PR. |
| #85 Ninja | C++ Ninja 1.13.1 at `79feac0`, GoogleTest 1.16.0, GCC 11.5.0 | Supported suite matches across two strict runs. | Full child-process cases hit `CLONE_VFORK`; PR body and metadata disagree on 378/32 versus 397/13 supported/excluded counts. |
| #97 Python stdlib | Meta fbpython 3.12.13+meta | Five modules/539 cases pass via a custom unittest driver. | Not OSS Python; launcher hits `CLONE_VFORK`, regrtest path exits 139. |
| #98 compression | C implementations, system bzip2 1.0.8 and gzip 1.12 | Three strict compressed-output hashes match. | System binary versions are not pinned; open PR. |
| #99 Redis | C Redis system 6.2.22 plus pinned Redis 7.2.4; Tcl 8.6 | Fast server/CLI subset plus ignored source suite. | Upstream Tcl suite reaches a `pselect6` timeout; open PR. |
| #101 SQLite | C/Tcl SQLite 3.51.2 source; system SQLite 3.34.1 | Fast WAL/transaction/index workload deterministic; native upstream has 330,902 assertions. | Strict full suite has 13 reproducible failures and stops before later lock tests. |
| #104 FP reduction | C/OpenMP, GCC/libgomp selected on host | Six strict IEEE-754 outputs match. | Open PR and compiler version not pinned in the test contract. |
| #107 Python hash seed | OSS CPython, host `/usr/bin/python3` | Native set order varies; strict output matches. | Open PR; must continue rejecting fbpython. |
| Shared-futex experiment | Node 16.20.2, Temurin Java 8u492, GCC 11.5.0 | Node workers 3/3, JVM threads 3/3, pthreads 5/5 on PR #53 commit. | Untracked and implementation not present on audited branch. |

## Priority Gaps

1. Add modeled or explicitly deterministic handling for `pread64` and
   `gettid`, then rerun all six runtimes with fail-closed syscall handling.
2. Merge a portable OSS Python test and keep `/usr/bin/python3`/version checks;
   do not let PATH select fbpython.
3. Decide whether the six runtime tests should become required CI. Doing so
   requires explicit, versioned toolchain setup rather than host discovery.
4. Port the Go lit test and remaining Buck language guests into Cargo.
5. Resolve `CLONE_VFORK` and `pselect6` blockers before claiming full Ninja,
   fbpython, or Redis upstream-suite coverage.
6. Reconcile Ninja PR #85's evidence metadata with its PR summary before using
   its test counts in release claims.
7. Preserve compiler/runtime versions in result metadata for every OSS
   workload; source revision alone is insufficient for reproducibility.

## Audit Limitations

- This is a source, harness, and observed-runtime audit, not execution of every
  imported rr test or every open PR workload.
- Normal strict-mode equality was measured only for the focused entropy
  programs. External filesystems and networks remain outside Hermit's stated
  determinism boundary.
- The complete internal Buck matrix is not available through public Cargo, so
  legacy lit source counts must not be equated with executed Cargo tests.
