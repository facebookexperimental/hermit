# Targeted determinism stress tests

These scripts exercise the ptrace backend at strict L2. Every test case invokes
the exact verifier path:

```text
hermit --log info run --strict --verify -- PROGRAM [ARGS...]
```

No determinism relaxations are used. Native executions run first and print the
SHA-256 of each output, making host nondeterminism visible without making a
probabilistic native difference a test prerequisite. The Hermit phase requires
the explicit `Determinism verified` marker.

Build Hermit once, then run a category or the complete matrix:

```bash
cargo build --release -p hermit --bin hermit
tests/e2e/lib/determinism-stress/random.sh
tests/e2e/lib/determinism-stress/run.sh
```

`DETERMINISM_STRESS_REPETITIONS=20` repeats every internal two-run comparison
twenty times for L4 stress evidence. The default is one L2 comparison so the
full targeted matrix remains practical. Other controls are:

```text
HERMIT_BIN                           release Hermit path
CC                                  host C compiler
NATIVE_STRESS_REPETITIONS           native demonstrations (default 3)
DETERMINISM_STRESS_TIMEOUT          seconds per native/verify run (default 180)
KEEP_DETERMINISM_STRESS_ARTIFACTS=1 retain logs under target/
```

## Coverage

| Script | Determinism surface |
| --- | --- |
| `examples.sh` | Every executable/program file in `examples/`; the manifest fails closed when a new example appears. |
| `random.sh` | `getrandom`, `/dev/random`, `/dev/urandom`, glibc/Python PRNGs, `secrets`, and `SystemRandom`. |
| `thread-racing.sh` | Barriers, mutex/condvar contention, rwlocks, semaphores, cancellation, TLS across fork, and C11 lock-free CAS/fetch-add contention. |
| `time-clock.sh` | `gettimeofday`, realtime/monotonic/process/thread/boottime `clock_gettime`, `clock_nanosleep`, and formatted timestamps. |
| `pid-tid.sh` | Virtual PID, PPID, and TID values across pthreads and fork/wait. |
| `signals.sh` | Pending/delivered signal order, masks, alternate stacks, reentrancy, and interrupted/restarted blocking syscalls. |
| `pipe-chain.sh` | Concurrent producers flowing through a four-stage Bash `A | B | C | D` pipeline. |
| `syscalls.sh` | Separate strict-verifier invocations for the syscall groups below. |

The syscall guests assert Hermit-specific policy as well as Linux semantics;
some intentionally reject behavior that succeeds natively. Native variation is
therefore demonstrated by the random, thread, time, PID/TID, signal, pipeline,
and examples categories rather than by treating those policy probes as native
portability tests.

### Targeted syscall groups

| Guest | Principal syscalls/APIs |
| --- | --- |
| `syscall_quick_wins.c` | `getresuid`, `getresgid`, `mmap`, `munlock`, `munlockall`, `munmap`, `open`, `write`, `fsync`, `sendfile`, `close_range`, `fcntl`, `seccomp`, `socketpair`, `shutdown`. |
| `syscall_file_io.c` | `open`, `write`, `fallocate`, `fstat`, `close`, `truncate`, `stat`, `rename`, `renameat`, `symlinkat`, `readlinkat`, `unlink`. |
| `syscall_file_metadata.c` | `pread`, `pwrite`, `fchmod`, `fchown`, `fchownat`, `faccessat`, `fchmodat2`, link/symlink and xattr families, `msync`, `readahead`, `sync_file_range`. |
| `writev_determinism.c` | `writev`/vectored I/O. |
| `epoll_determinism.c` | `epoll_create1`, `epoll_ctl`, `epoll_wait`. |
| `mmap_determinism.c` | Anonymous/file mappings, protection changes, remapping, and unmapping. |
| `resource_determinism.c` | Resource limits and usage queries. |
| `ipc_determinism.c` | Pipes, polling, eventfd/signalfd-style IPC paths covered by the existing guest. |
| `arch_prctl_determinism.c` | Deterministic architecture-control state. |
| `getitimer_determinism_probe.c` | `getitimer`. |
| `setitimer_determinism.c` | `setitimer` plus deterministic timer state. |
| `timer_create_determinism.c` | `timer_create`, `timer_settime`, `timer_gettime`, `timer_getoverrun`, and `timer_delete`. |

This is targeted syscall coverage, not a claim that every Linux syscall is
implemented or tested. Unsupported/refused syscall contracts continue to live
in their dedicated C guests and Rust integration tests.

## Internal-suite audit

The 2026-07-27 audit compared `tests/` with
`fbsource/fbcode/hermetic_infra/hermit/tests/`:

- Internal tree: 144 files.
- Public tree before this change: 264 files.
- All 135 internal non-build-metadata paths are present in the public tree.
- The nine internal-only paths are `BUCK`/`PACKAGE` files.
- The public tree has 129 additional paths, including backend-parity, strict
  refusal, procfs, timer, QEMU, and compatibility coverage.

File parity is not target parity: the internal Buck matrix combines these
guests into hundreds of configurations that are not all represented by public
Cargo targets. Detcore's Rust test infrastructure provides
`make_det_test_variants!` and direct `#[test]` cases for syscall semantics in
`detcore/tests/{misc,time,...}`, including Jason White's historical lit-test
and scheduler-affinity work. There is no generated one-test-per-syscall
completeness manifest. This shell suite therefore adds explicit strict L2
coverage at the CLI boundary rather than treating green unit tests as complete
end-to-end syscall coverage.
