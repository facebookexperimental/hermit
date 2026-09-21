# SaBRe backend compatibility

[SaBRe](https://github.com/srg-imperial/SaBRe) is a load-time selective binary
rewriter that intercepts a program's system calls from inside the process,
rather than through the kernel-mediated tracing that `ptrace` uses. Hermit can
use it as an experimental execution backend for `run`: it loads the shared
Detcore determinism engine into a SaBRe plugin while a Hermit coordinator owns
Detcore's global state. It is useful for measured workloads, but it is not a
drop-in replacement for the ptrace backend.

This document describes the post-0.2 work-ahead envelope. The executable test
manifests are the source of truth: a SaBRe entry in `backends_enabled` means the
named cell matched under SaBRe's `Stripped` comparison and passed its stated
semantic oracle. An exclusion remains a support gap, not an implied fallback
to ptrace.

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

## Trusted shared-object path evidence

The E2E SaBRe contract requires two complete path-evidence records for a verify
cell, with coordinator RPC observed and both `ptrace_fallback_sites` and
`trusted_shared_object_sites` equal to zero. A canonical comparator match does
not override that execution-path requirement.

The disabled `language-runtimes/perl-io-subprocess-time` probe demonstrates
why. On 2026-08-17 its no-relaxation comparison matched, but each verify pair
recorded 132 trusted shared-object sites: 33 distinct raw syscall instructions
in `libc` and `ld-linux`, in both the Perl process and its `/usr/bin/tr` child,
across both verify runs. The sites included `clock_gettime`, `getrandom`, file
I/O, memory mapping, and signal operations. Those instructions executed
outside the measured SaBRe interception path.

Forcing ordinary shared-object sites through the existing SaBRe marker did not
turn that result into complete evidence. Three attempts all produced
`no_result`: the first `ld-linux` site terminated with `SIGILL`. The cell can be
qualified only after shared-object rewriting or marker handling lets the exact
guest complete canonical verification with zero trusted and zero fallback
sites.

This probe was never among the preserved 53 published-green SaBRe cells. A
separate audit of that exact 53-cell population, using Hermit binary SHA-256
`9b06079a7f869951b1d1c3aa9ec8d2c187e2408d730cc7cb2879a71150fac118`,
found zero trusted and zero fallback sites in every complete verify pair. The
Perl finding therefore does not widen to those 53 cells.

## Measured `Stripped` envelope

Baseline sweep provenance:

- Runtime implementation base: Hermit
  `0ca0dec256fd484e238b475a031a5c2d482eeba8` (version 0.2.0), Reverie dependency
  `adc147342f34754b449b9a24174aca3ac3a2e16b`.
- SaBRe loader: `80883b80a74d9c649419bdacc97dfd146baa34df`,
  SHA-256 `cd0b75ed6f585a2447675a9b74577a3ec643489615a3549f9e95ca4705893418`.
- Host: Linux `6.18.39-0_fbk0_hardened_0_ga43d5727b443`, AMD EPYC 9D85,
  `perf_event_paranoid=1`.
- Toolchain: `rustc 1.99.0-nightly (26ae60a9e 2026-07-28)`.
- Comparator: `Stripped` (`run --backend sabre --strict --verify`). The portable
  corpus uses `--no-virtualize-cpuid --max-timeslice=disabled`; the standalone
  `/bin/echo`, `/bin/true`, and `/bin/cat /dev/null` probes matched under
  `Stripped` without additional relaxations. These results are not L2.
- Log level: INFO for verification. Every cell was bounded by its manifest
  timeout. `race.sh` was not run.

The incremental `arch-prctl-determinism` qualification uses Hermit feature
base `1ece0654e39c67fa0555dfde645a8da61eb2f059`, Reverie candidate
`8a3a15e01b5678715fdc9dcb316f1f411f44d0e3`, and SaBRe candidate
`2a54b65f6d83d6e26606f8402a9dfb2a9cf82e5e`. The staged release loader
SHA-256 is `22b68605cd2f01922f3f566fc05dc76ba916639aa654054a46fdccf5e2e41744`;
the Hermit binary is
`48d79ea85d92933a7bb607f56a62eaadc71a1921ca6d2f832193d5f0e2955997`
and `libdetcore_sabre.so` is
`a9a3bcfd435ca350f3f2c8a0bb0ee9fadeec4737bcb373e4b47e883a71bbe9fc`.
The cell produced a SaBRe `Stripped` match with 9/9 selected
DETLOG/scheduler-COMMIT messages matching after `Stripped` normalization. Its
ptrace and SaBRe guest output was independently byte-identical (SHA-256
`8504ad2cf53c948ffdd59e277fe87ecf21f65ffa4fb543989366ec9cb40272fd`).

This isolated cell does not change the separate 212-program compatibility
corpus measurement. Its latest retained `Stripped` run remains 207/212
(97.64%) at Hermit `c4b7b1a6dc4c1bfe1f03b68ec5d2efa991d9256b`; `gcc`, `g++`, and
`cpp` timed out, `java` had a substantive DETLOG mismatch, and `timeout` failed
its first run. The five gaps and the `Stripped` comparator preclude B3, B4, L2,
or any canonical parity claim.

The initial post-0.2 ptrace verification plan had 194 cells. Before that
ratchet, SaBRe was enabled for 22 (11.3%). This ratchet evaluates 157
previously disabled C candidates:

| Result | Cells | Meaning |
| --- | ---: | --- |
| SaBRe `Stripped` match and ptrace exit/stdout parity | 110 | Enabled by this ratchet |
| SaBRe `Stripped` match, but ptrace output differs | 18 | Remains disabled |
| SaBRe `Stripped` comparison failed or timed out | 29 | Remains disabled |

The resulting plan enables SaBRe for 132/194 cells (68.0%): seven blocking CI
cells and 125 manual cells. This meets the numerical threshold formerly used
for the B3 corpus count, but the underlying comparisons were `Stripped`; it
does not establish B3, L2, B4, L3 memory determinism, L4 stress hardening, or
support for every workload in a subsystem.

The 110 newly enabled cells are grouped as follows:

| Manifest bucket | New cells |
| --- | ---: |
| `c-programs` | 100 |
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
gives `backend-parity-c/pid-probe` and `debugger-c/debuggee` SaBRe `Stripped`
matches with byte-identical ptrace output under the portable profile. It does
not establish L2 or claim parity for child/thread identities, whose backend
task topologies still differ.
The socket-cookie increment gives sockets their own per-task open sequence.
Linux specifies a nonzero identity that is unique among live sockets and shared
by descriptor aliases, but does not specify its numeric value. Keeping the
socket sequence separate from regular-file opens preserves those properties and
prevents ptrace-only dynamic-linker file operations from shifting SaBRe-visible
cookies. This gives `c-programs/socket-cookie-tcp`,
`c-programs/socket-cookie-udp`, and `c-programs/socket-cookie-unix` SaBRe
`Stripped` matches with byte-identical ptrace output under the portable profile;
it does not establish L2.

At this increment's source tree, the executable plan enables SaBRe for 133/200
ptrace verify cells (66.5%) under `Stripped`: seven blocking-CI cells and 126
manual cells. This count does not establish B3 or L2. It is up by three cells
from the live `origin/main` plan's 130/200 (65.0%); the denominator and enabled
set have changed since the historical 133/199 root-process-identity snapshot
above.

## Known gaps

The historical output-differ audit contained 18 cells. Five now have
byte-identical ptrace/SaBRe output and are enabled. Ten remain under the owners
of clock, multithreaded-random, SIGCHLD, or multithreaded-identity semantics.
The three remaining non-gated cells match under SaBRe's `Stripped` comparison
but stay disabled because their guest output is still backend-specific:

| Cell | Disposition | Evidence |
| --- | --- | --- |
| `backend-parity-c/pid-probe` | Fixed and promoted | Root PID alignment makes ptrace and SaBRe output byte-identical. |
| `c-programs/dbt-pid-virtualization` | Blocked | Child allocation and vfork/exec behavior still expose different backend task topologies. |
| `c-programs/print-memaddrs` | Blocked | SaBRe relocation changes the stack, brk heap, and large-allocation addresses. |
| `c-programs/proc-fdinfo` | Blocked | Loader-visible regular-file opens shift the virtual inode: ptrace reports 3 and SaBRe reports 1. |
| `c-programs/random-sources` | Owner-gated | Multithreaded random ordering belongs to the MT-random owner. |
| `c-programs/setitimer-determinism` | Owner-gated | The mismatch is part of cross-backend virtual-clock trajectories. |
| `c-programs/sigtimedwait-timeout-1s` | Owner-gated | The mismatch is part of cross-backend virtual-clock trajectories. |
| `c-programs/socket-cookie-tcp` | Fixed and promoted | The socket-only open sequence makes ptrace and SaBRe output byte-identical. |
| `c-programs/socket-cookie-udp` | Fixed and promoted | The socket-only open sequence makes ptrace and SaBRe output byte-identical. |
| `c-programs/socket-cookie-unix` | Fixed and promoted | The socket-only open sequence makes ptrace and SaBRe output byte-identical. |
| `c-programs/socket-timestamp-edge-cases` | Owner-gated | The mismatch is part of cross-backend virtual-clock trajectories. |
| `c-programs/socket-timestamp-timespec` | Owner-gated | The mismatch is part of cross-backend virtual-clock trajectories. |
| `c-programs/socket-timestamp-timeval` | Owner-gated | The mismatch is part of cross-backend virtual-clock trajectories. |
| `c-programs/sysinfo` | Owner-gated | Uptime and memory observations are owned with guest-clock/vtime semantics. |
| `c-programs/sysinfo-uptime` | Owner-gated | The mismatch is part of cross-backend virtual-clock trajectories. |
| `c-programs/wait-on-child` | Owner-gated | Child completion ordering belongs to the SIGCHLD owner. |
| `debugger-c/debuggee` | Fixed and promoted | Root PID alignment makes ptrace and SaBRe output byte-identical. |
| `determinism-stress-c/pid-tid` | Owner-gated | Thread identity allocation belongs to MT identity and scheduling. |

The three non-gated probes were rerun at Hermit
`cc026964cf8b992ecd95883418991571783799c0` with Reverie
`aa6f1283aeee3efd174c57f6dd8198310bd307e1`. All three matched under SaBRe's
`Stripped` comparison at INFO log level, but direct ptrace/SaBRe stdout
comparison failed for all three. This audit therefore makes no manifest
promotion: the plan remains 133/200 before and after it. These results are not
L2.

Separately, the same full scorecard found that the already-enabled
`c-programs/mmap-determinism` matched under `Stripped` on both backends but had
`stdout_parity=false`. That regression is outside the historical 18-cell set
and requires independent requalification; it is not counted as progress here.
The result is not L2.

### `startup-tls-guards`: the first divergent record is stdout content

The imported canonical measurements for
`system-utils/startup-tls-guards/verify@sabre` diverged three times out of
three at scheduler turn 3, virtual time `1767225600002223000`, record 117, and
syscall 48. This establishes reproduction at a stable coordinate, not a rate.
The raw log pairs for those three measurements are no longer present, so their
two exact output-buffer hashes cannot be recovered from the imported summary.

A later focused canonical run at Hermit
`1540f91a0539e0cec8923d33220cdc316c910a0b` retained both logs. The comparison
surface had shifted by two records and one syscall, but the differing record
was the same operation: the output-buffer record for the guest's final
114-byte stdout write. The adjacent records were identical in both runs:

```text
finish syscall #48: madvise(0x5c3d4780000, 196608, 4) = Ok(0)
inbound syscall: write(1, 0x5555555712a0, 114) = ?
finish syscall #49: write(1, 0x5555555712a0, 114) = Ok(114)
```

The following output-buffer records differed:

```text
run 1: DETLOG [iobuf][dtid 2] write out fd=1 0x5555555712a0+114->2b2f1cc110e6ff75af9c76ddfc1e578b93da28501a387973732482106042011a
run 2: DETLOG [iobuf][dtid 2] write out fd=1 0x5555555712a0+114->3df75cedc1078091f736c9bb24bbe671456f3705c60737bc72172d63db4a0c82
```

Those hashes are the SHA-256 values of these exact guest outputs:

```text
run 1:
STACK_CANARY 0x6df6042a2bc69b00
POINTER_GUARD 0xf659c12325d8b3af
AT_RANDOM_BYTES a2cd18d300537a5cb083dc48dbfa0ef2

run 2:
STACK_CANARY 0xc692cfb823819a00
POINTER_GUARD 0xb1159e693df08f44
AT_RANDOM_BYTES a2cd18d300537a5cb083dc48dbfa0ef2
```

The next record is the identical `exit_group(0)` entry. Both executions also
have the same four scheduler turns, 50 completed syscalls, final virtual time
`2733000`, and complete SaBRe path evidence: guest RPC observed, zero ptrace
fallback sites, and zero trusted shared-object sites. The differing fact is the
stdout content alone: both glibc TLS guards vary while the later read of
`AT_RANDOM` matches.

This is not the pthread exit/join mechanism behind
`backend-parity-c/pthread-lifecycle/verify@sabre` and
`chaos-c/lock-granularity/verify@sabre`. Those cells first differ in a scheduler
COMMIT's virtual time by exact multiples of the 5,000 ns futex charge, around
different `pthread_join` futex sequences. This cell has one guest thread, no
such futex sequence, identical scheduler/runtime totals, and first differs in
the final stdout bytes.

The source ordering explains the output. `tests/c/startup_tls_guards.c` reads
the x86-64 glibc stack canary at `%fs:0x28`, the pointer guard at `%fs:0x30`,
and the 16 bytes addressed by `AT_RANDOM`. Detcore's `handle_post_exec` replaces
those auxiliary-vector bytes with deterministic bytes. At the pinned Reverie
commit `86d9003a7a2a8d5399ef94a251e4d991d6c504a5`, however, SaBRe records a
pending post-load event and does not deliver it to Detcore until the first
rewritten syscall. Glibc has initialized the guards before that callback. The
SaBRe loader also handles `ARCH_SET_FS` before the plugin exists and copies its
stack guard at offset `0x28` into the client TLS; it has no corresponding
pointer-guard handling. By the time Detcore writes deterministic `AT_RANDOM`
bytes, both printed guards have already been initialized from the earlier
startup state.

An exhaustive source search finds no other test reading `%fs:0x28` or
`%fs:0x30`; `system-utils/startup-surface-identity` reads `AT_RANDOM` itself but
not the guards. No other confirmed divergent cell currently demonstrates this
mechanism. Repair belongs before glibc consumes the initial auxiliary vector,
not in the later Detcore callback and not by rewriting live guards after
protected frames may exist.

The retained focused artifacts had these hashes before their relevant records
were copied here:

| Artifact | SHA-256 |
| --- | --- |
| run 1 log | `17f413cb344daa4faee2b67ac4d78c58f34f19a4ce1c000bdbe23dafc4c82c75` |
| run 2 log | `95b3df52b686185bf73b73779de62d0813ffa7b7cccd2380654862e0c313e184` |
| comparison stderr | `d4086e0d8fc134358e96c05fb0a9878f24e2cedbbae179f25eb24ed8e29b91a1` |
| verification report | `d039974bb3dc0df037adfcf7845c4ebb7c642f91404c220d16dacd3249b7660d` |
| result row | `096db88e9e74548e736a32466b07e74089303bb0ce6083bc4ac1fb268d31c867` |

The fixture, Detcore post-exec code, SaBRe adapter, pinned Reverie revision, and
manifest row are byte-identical between that focused run and Hermit main
`538ddb26bb2ce92d9cc9bf4303d4e0f9602517a5`. No fresh guest run was taken for
this documentation update while another full validate owned the machine.

The following 27 candidates fail SaBRe's `Stripped` comparison or its timeout
and remain disabled:

```text
backend-parity-c/cpuid-probe
bin-c/robust-futex-test
c-programs/clone
c-programs/dbt-unsupported-syscall
c-programs/fp-reduction-nondeterminism
c-programs/hello-nostdlib
c-programs/ipc-determinism
c-programs/liteinst-advanced
c-programs/nanosleep-threads-simple
c-programs/pread64-nostdlib
c-programs/pselect6-simulation
c-programs/racewrite-nostdlib
c-programs/record-replay-file-state
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
  Detcore. The named `patch` workload produced five consecutive `Stripped`
  matches on the measured Fedora host, but a GitHub Ubuntu package
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
target/debug/test-harness validate
target/debug/test-harness run --lane portable --backend sabre --ci-only
```

Run a manual enabled cell with its exact ID:

```bash
target/debug/test-harness run --include-manual --mode verify \
  --backend sabre --test c-programs/syscall-file-io
```

All Hermit namespace runs require a host that permits the required user, PID,
mount, and network namespaces.
