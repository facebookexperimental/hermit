# SaBRe backend compatibility

SaBRe is an experimental Linux x86-64 execution backend for Hermit `run`. It
loads the shared Detcore implementation into a SaBRe plugin while a Hermit
coordinator owns Detcore's global state. It is useful for measured workloads,
but it is not a drop-in replacement for the ptrace backend.

This document describes the post-0.2 work-ahead envelope. The executable test
manifests are the source of truth: a SaBRe entry in `backends_enabled` means the
named cell passed both SaBRe strict verification and its stated semantic
oracle. An exclusion remains a support gap, not an implied fallback to ptrace.

## Build and run

SaBRe is behind the non-default `third-party-backends` Cargo feature and needs
the staged loader plus `libdetcore_sabre.so` beside the Hermit executable:

```bash
cargo build --release --locked -p hermit \
  --features third-party-backends -p detcore-sabre
HERMIT_INSTALL_FORCE_RESTAGE=local-sabre \
  cargo build --release --locked -p hermit-install

target/release/hermit run --backend sabre --strict --verify -- /bin/echo hello
```

An explicit SaBRe request fails closed if the feature or artifacts are absent.
Hermit does not silently substitute ptrace.

## What the backend actually executes

The CLI path is:

```text
run_with_backend_inner(Backend::Sabre)
  -> run_sabre
  -> detcore::GlobalState in the Hermit coordinator
  -> detcore-sabre::Plugin implementing reverie_sabre::Tool
  -> RemoteReverieAdapter<Detcore> / SabreGuest
  -> shared Detcore syscall and scheduler logic
```

This path is visible in INFO logs as
`launching Detcore guest through SaBRe with coordinator RPC`, followed by
`detcore::scheduler` commit turns. SaBRe uses a plugin/guest adapter rather
than a generic `impl reverie::Backend`; the architectural difference does not
create a second determinism engine.

## Measured strict-verify envelope

Snapshot:

- Runtime implementation base: Hermit
  `0ca0dec256fd484e238b475a031a5c2d482eeba8` (version 0.2.0), Reverie dependency
  `adc147342f34754b449b9a24174aca3ac3a2e16b`.
- SaBRe loader: `80883b80a74d9c649419bdacc97dfd146baa34df`,
  SHA-256 `cd0b75ed6f585a2447675a9b74577a3ec643489615a3549f9e95ca4705893418`.
- Host: Linux `6.18.39-0_fbk0_hardened_0_ga43d5727b443`, AMD EPYC 9D85,
  `perf_event_paranoid=1`.
- Toolchain: `rustc 1.99.0-nightly (26ae60a9e 2026-07-28)`.
- Level: L2 (`run --backend sabre --strict --verify`). The portable corpus
  uses `--no-virtualize-cpuid --max-timeslice=disabled`; the standalone
  `/bin/echo`, `/bin/true`, and `/bin/cat /dev/null` probes pass L2 without
  relaxations.
- Log level: INFO for verification. Every cell was bounded by its manifest
  timeout. `race.sh` was not run.

The initial post-0.2 ptrace strict-verify plan had 194 cells. Before that
ratchet, SaBRe was enabled for 22 (11.3%). This ratchet evaluates 157
previously disabled C candidates:

| Result | Cells | Meaning |
| --- | ---: | --- |
| SaBRe L2 and ptrace exit/stdout parity | 109 | Enabled by this ratchet |
| SaBRe L2, but ptrace output differs | 18 | Remains disabled |
| SaBRe L2 failed or timed out | 30 | Remains disabled |

The resulting plan enables SaBRe for 131/194 cells (67.5%): seven blocking CI
cells and 124 manual cells. This meets the B3 corpus-count threshold (at least
50% of the ptrace strict-verify corpus). It does not establish B4, L3 memory
determinism, L4 stress hardening, or support for every workload in a subsystem.

The 109 newly enabled cells are grouped as follows:

| Manifest bucket | New cells |
| --- | ---: |
| `c-programs` | 99 |
| `determinism-stress-c` | 7 |
| `backend-parity-c` | 1 |
| `bin-c` | 1 |
| `chaos-c` verify mode | 1 |

The exact allowlist is available with:

```bash
cargo run --locked --quiet -p hermit-manifest-plan -- --format json \
  | jq '.[] | select(.backend == "sabre" and .mode == "verify")'
```

Representative coverage includes dynamically linked process/thread lifecycle,
file and procfs metadata, memory mapping, timers, PIDFD, poll, netlink and UNIX
socket autobind, TCP info, syscall-refusal semantics, pipes, fork trees, shared
mappings, and signal ordering. These are probe-specific claims; for example,
some fork and signal probes pass while other probes in those categories do not.

The root-process identity increment starts the SaBRe tracee before creating its
blocking ptrace-supervisor worker. Linux assigns the guest namespace PID 3,
matching ptrace, instead of assigning 3 to the worker and 4 to the guest. This
qualifies `backend-parity-c/pid-probe` and `debugger-c/debuggee` at SaBRe L2
with byte-identical ptrace output under the portable profile. It does not claim
parity for child/thread identities, whose backend task topologies still differ.
The socket-cookie increment gives sockets their own per-task open sequence.
Linux specifies a nonzero identity that is unique among live sockets and shared
by descriptor aliases, but does not specify its numeric value. Keeping the
socket sequence separate from regular-file opens preserves those properties and
prevents ptrace-only dynamic-linker file operations from shifting SaBRe-visible
cookies. This qualifies `c-programs/socket-cookie-tcp`,
`c-programs/socket-cookie-udp`, and `c-programs/socket-cookie-unix` at SaBRe L2
with byte-identical ptrace output under the portable profile.

At this increment's source tree, the executable plan enables SaBRe for 133/200
ptrace verify cells (66.5%, B3): seven blocking-CI cells and 126 manual cells.
That is up by three cells from the live `origin/main` plan's 130/200 (65.0%);
the denominator and enabled set have changed since the historical 133/199
root-process-identity snapshot above.

## Known gaps

The following 13 cells are deterministic inside SaBRe but do not match ptrace
guest output, so they remain disabled:

```text
c-programs/dbi-pid-virtualization
c-programs/print-memaddrs
c-programs/proc-fdinfo
c-programs/random-sources
c-programs/setitimer-determinism
c-programs/sigtimedwait-timeout-1s
c-programs/socket-timestamp-edge-cases
c-programs/socket-timestamp-timespec
c-programs/socket-timestamp-timeval
c-programs/sysinfo
c-programs/sysinfo-uptime
c-programs/wait-on-child
determinism-stress-c/pid-tid
```

The following 30 candidates fail SaBRe strict verification or its timeout and
remain disabled:

```text
backend-parity-c/cpuid-probe
bin-c/robust-futex-test
c-programs/arch-prctl-determinism
c-programs/clone
c-programs/dbi-unsupported-syscall
c-programs/epoll-determinism
c-programs/fp-reduction-nondeterminism
c-programs/hello-nostdlib
c-programs/ipc-determinism
c-programs/liteinst-advanced
c-programs/nanosleep-threads-simple
c-programs/pread64-nostdlib
c-programs/pselect6-simulation
c-programs/racewrite-nostdlib
c-programs/record-replay-file-state
c-programs/record-replay-lseek-seek-cur
c-programs/resource-determinism
c-programs/signal-determinism
c-programs/sigpipe-siginfo
c-programs/sigtimedwait-no-timeout
c-programs/socket-ioctl-timestamp
c-programs/thread-sync-determinism
c-programs/vforkexec
c-programs/writev-determinism
determinism-stress-c/thread-contention
shared-futex-c/qemu-exec-init
shared-futex-c/qemu-hello
shared-futex-c/qemu-init
shared-futex-c/qemu-net-init
util-c/pmu-skid
```

Additional backend-wide limits:

- GNU `patch` reaches `getrandom` through glibc at a libc site that the SaBRe
  syscall rewriter can miss. The plugin detours that libc function through
  Detcore. The canonical `patch` workload passed five consecutive strict
  verification probes on the measured Fedora host, but a GitHub Ubuntu package
  still reached a different libc-internal random path and varied its temporary
  suffix. Portable CI therefore covers a compiled public-`getrandom` caller;
  it does not claim every host `patch` build is deterministic. This also does
  not close the broader random-source gap: the multithreaded `random-sources`
  probe still produces different ptrace and SaBRe stdout and DETLOG streams
  and remains disabled.
- The exhaustive `relaxed_flag_matrix` integration test is currently a
  ptrace-only cross-product. It exercises getrandom in its observation guest,
  but provides no SaBRe flag-matrix coverage; adding a bounded SaBRe slice is a
  separate qualification batch.
- SaBRe supports deterministic `run` and the narrow SaBRe `strace` command;
  record/replay and chaos scheduling are unsupported.
- `race.sh` is excluded. SaBRe does not serialize arbitrary guest instructions
  between callbacks, so a callback-only result would not prove schedule parity.
- CPUID and RDTSCP are not fully intercepted. The clock-determinism cell remains
  disabled because raw host TSC can leak through RDTSCP.
- Continuous virtual time is deterministic within each backend, but the ptrace
  and SaBRe clock trajectories are not yet identical.
- Static/no-libc binaries are outside the current rewrite envelope, as shown by
  `hello-nostdlib`, `pread64-nostdlib`, and `racewrite-nostdlib`.
- Process and thread support is selective: fork-tree and several lifecycle
  probes pass, while raw clone, vfork/exec, robust-futex, and some contention
  probes fail or time out.

## Reproducing the gates

Validate manifest policy and run the blocking SaBRe cells:

```bash
./ci/test_harness.sh validate
./ci/test_harness.sh run --lane portable --backend sabre --ci-only
```

Run a manual enabled cell with its exact ID:

```bash
./ci/test_harness.sh run --include-manual --mode verify \
  --backend sabre --test c-programs/syscall-file-io
```

All Hermit namespace runs require a host that permits the required user, PID,
mount, and network namespaces.
