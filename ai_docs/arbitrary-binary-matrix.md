# Arbitrary Binary Compatibility Matrix

Last tested: 2026-07-21

This report measures how unmodified host binaries behave under Hermit. It
separates a cheap launch/version matrix from functional workloads that exercise
subprocesses, files, sockets, and threads. The launch matrix is also represented
by a public Cargo integration test so regressions in the passing subset are
caught on the self-hosted CI runner.

## Test environment

- Hermit: `5b9a2d31411695140d628664dd67fa67799dd08d`
- Reverie: `96693397ed60aa07c59ffeed4df3deed89b183e2`
- OS: CentOS Stream 9, Linux 6.13.2 x86_64
- CPU: AMD EPYC 9D85
- CPUID faulting: unavailable on this host
- Preemption: disabled for launch tests so they do not require PMU access

Hermit was built with `cargo build -p hermit`. Run-mode probes used:

```bash
target/debug/hermit run \
  --base-env=minimal \
  --no-virtualize-cpuid \
  --preemption-timeout=disabled \
  -- PROGRAM ARGS...
```

Record and replay were tested as distinct phases:

```bash
target/debug/hermit record start --data-dir=DATA -- PROGRAM ARGS...
target/debug/hermit replay --autopilot --data-dir=DATA
```

`PASS` means the command exited zero and emitted its expected marker. `FAIL`
means a reproducible nonzero exit or replay panic. `HANG` means it exceeded the
timeout and required cleanup. `BLOCKED` means no valid recording was available
to replay. `N/A` means the package was not installed.

## Launch and version matrix

| Category | Probe | Run | Record | Replay | Notes |
| --- | --- | --- | --- | --- | --- |
| Static ELF | BusyBox `echo` | PASS | PASS | PASS | Statically linked baseline. |
| Dynamic ELF | GNU `ls --version` | PASS | PASS | PASS | Dynamically linked baseline. |
| Shell | `sh -c printf` | PASS | PASS | PASS | Shell built-in, no child process. |
| Python | System Python `print` | PASS | PASS | PASS | `/usr/bin/python3`; complex imports differ below. |
| Python | Meta Python `print` | HANG | HANG | BLOCKED | Uses unsupported `CLONE_VFORK`; issue #15. |
| Node | Node 16 `console.log` | PASS | HANG | HANG | Recording is incomplete; replay also leaks stopped processes; issue #19. |
| Java | OpenJDK 8 `java -version` | PASS | PASS | HANG | Replay outlives `timeout -k`; issue #19. |
| Go | Go 1.26 `go version` | PASS | PASS | FAIL | Unexpected `Return(3)` event; issue #31. |
| HTTP client | `curl --version` | PASS | PASS | PASS | No network in this probe. |
| HTTP client | `wget --version` | PASS | PASS | PASS | No network in this probe. |
| VCS | System Git `--version` | PASS | PASS | PASS | `/usr/bin/git`; functional Git differs below. |
| VCS | Meta Git `--version` | HANG | HANG | BLOCKED | Uses unsupported `CLONE_VFORK`; issue #15. |
| C compiler | GCC `--version` | PASS | PASS | PASS | Functional compilation differs below. |
| Build tool | Make `--version` | PASS | PASS | PASS | Functional builds invoke `CLONE_VFORK`. |
| Build tool | CMake `--version` | N/A | N/A | N/A | CMake was not installed on the test host. |
| Rust | Direct toolchain Cargo `--version` | PASS | PASS | PASS | Resolved with `rustup which cargo`. |
| Rust | Rustup Cargo proxy `--version` | FAIL | FAIL | BLOCKED | Guest gets `EBADF`; issue #16. |
| Rust | Rustup `--version` | FAIL | FAIL | BLOCKED | Same inherited-fd defect as the Cargo proxy. |
| Database | SQLite in-memory query | PASS | PASS | FAIL | Unexpected `Mmap` event and secondary SIGSEGV; issue #31. |
| Server | Redis | N/A | N/A | N/A | `redis-server` was not installed. |
| Server | Nginx | N/A | N/A | N/A | `nginx` was not installed. |

The passing version probes are meaningful launch coverage, but they do not
prove that a tool's primary workflow works. Several tools fail only after they
create children, open additional files, or use sockets.

## Functional workload matrix

| Workload | Native | Run | Record/replay | Result |
| --- | --- | --- | --- | --- |
| `cat`, `grep`, `sed`, `awk` on a bound input | PASS | PASS | PASS | Host input must be visible inside the Hermit container. |
| GCC compile and execute a C program | PASS | PASS | FAIL | Replay syscall order diverges in `cc1`; issue #19. |
| Make build of the same C program | PASS | HANG | BLOCKED | Make reaches unsupported `CLONE_VFORK`; issue #15. |
| Direct Cargo build of a local crate | PASS | HANG | BLOCKED | Cargo reaches unsupported `CLONE_VFORK`; issue #15. |
| Cargo build through rustup proxy | PASS | FAIL | BLOCKED | Proxy fails with `EBADF`; issue #16. |
| Read-only system Git status | PASS | PASS | FAIL | Replay diverges between `openat` and `getcwd`; issue #19. |
| Python imports, file read, JSON, and hashing | PASS | PASS | FAIL | Recorder panics on `ioctl(FIOCLEX)`; issue #17. |
| Python subprocess | PASS | PASS | FAIL | Same `FIOCLEX` recording failure; issue #17. |
| Two-process shell pipeline | PASS | PASS | HANG | Replay syscall desync and stopped-child leak; issue #19. |
| Curl against a local HTTP server | PASS | PASS | FAIL | Replay fd/syscall desync; issue #19. |
| Wget against a local HTTP server | PASS | FAIL | FAIL | Blocking connect returns `EINPROGRESS`; issue #18. |
| Python loopback socket client/server | PASS | FAIL | BLOCKED | Blocking connect returns `EINPROGRESS`; issue #18. |
| Multithreaded signal stress binary | PASS | PASS | PASS | Threads and signal delivery pass this bounded workload. |
| SQLite in-memory query | PASS | PASS | FAIL | Filesystem-event replay mismatch; issue #31. |

External network access was used only as a stress observation and is not a
determinism guarantee. The actionable network reproductions use loopback. The
functional compiler/build cases also create output files; Hermit does not make
a changing filesystem deterministic. Issue #19 remains actionable because its
smallest pipeline reproducer neither uses the network nor changes files.

## Failure categories

The matrix maps to one issue per distinct failure category on the approved
`rrnewton/hermit` fork:

- [#15](https://github.com/rrnewton/hermit/issues/15): unsupported `clone(CLONE_VFORK)` hangs and stopped-process cleanup.
- [#16](https://github.com/rrnewton/hermit/issues/16): rustup proxies fail with `EBADF` under `hermit run`.
- [#17](https://github.com/rrnewton/hermit/issues/17): record panics on `ioctl(FIOCLEX)`.
- [#18](https://github.com/rrnewton/hermit/issues/18): blocking `connect` incorrectly returns `EINPROGRESS`.
- [#19](https://github.com/rrnewton/hermit/issues/19): record/replay syscall desynchronization and leaked stopped children. Java and Node hang results were added to this issue rather than filed as a duplicate.
- [#31](https://github.com/rrnewton/hermit/issues/31): Go and SQLite replay consume unexpected filesystem events.

The tested base does not contain the open `CLONE_VFORK` implementation in [PR #27](https://github.com/rrnewton/hermit/pull/27). Make, Cargo builds, Meta Python, and Meta Git should be retested after that change lands.

## CI coverage

`hermit-cli/tests/arbitrary_binaries.rs` adds two black-box tests:

- `run_arbitrary_binary_matrix` discovers installed tools and runs static and dynamic ELF, shell, Python, Node, Java, Go, curl, wget, Git, GCC, Make, CMake, SQLite, and a direct Cargo binary. Missing optional packages are skipped; `ls` and `sh` are required baselines.
- `record_replay_stable_arbitrary_binaries` runs `record start --verify` for the locally proven subset: BusyBox, `ls`, `sh`, system Python, curl, wget, system Git, GCC, Make, and direct Cargo.

The existing self-hosted CI job runs `cargo test -p hermit`, so Cargo discovers these integration tests without a workflow change. On this host the run test covered 15 installed categories in 1.9 seconds, and the stable record/replay test covered 10 categories in 16.4 seconds.

Keep failing cases out of the green CI matrix until their linked issues are fixed. When a fix lands, move the smallest corresponding probe into the stable record/replay set and retain the functional workload as regression coverage.
