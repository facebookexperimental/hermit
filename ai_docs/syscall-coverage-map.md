# Detcore x86_64 syscall coverage map

Status: research snapshot, 2026-07-21

## Executive summary

This audit uses Hermit revision
`592d5c6ccbced0d1240b6562ff87652cb706f142` and the host's
`/usr/include/asm/unistd_64.h` as the x86_64 ABI baseline. The header contains
382 named syscalls: numbers 0-335 and 424-469. The reserved gap 336-423 is not
counted.

| Classification | Count | Share of 382 |
| --- | ---: | ---: |
| DETERMINIZED | 69 | 18.1% |
| PASSTHROUGH | 19 | 5.0% |
| BLOCKED | 3 | 0.8% |
| MISSING | 291 | 76.2% |

"DETERMINIZED" includes partial models. Of those 69 entries, only
`getrandom` and `getcpu` are unconditional full replacements of their
nondeterministic outputs. Six more have complete deterministic output/effect
only in the relevant configured mode: `alarm`, `pause`,
`clock_gettime`, `clock_getres`, `time`, and `sched_yield`. The other
61 are partial models that still use the host kernel, cover only some commands
or file-descriptor types, or depend on a stable filesystem/network environment.

Release `hermit run` subscribes to 78 syscall numbers. That set comprises the
69 determinized entries, six explicit passthroughs, and three blocked entries.
Normal CLI defaults enable scheduling, deterministic I/O, time virtualization,
and metadata virtualization, so all conditional subscription groups are active
unless the user opts out. A debug build instead uses `Subscription::all()`.

The largest correctness issue is fail-open coverage. In an optimized run, an
unsubscribed syscall never reaches `handle_syscall_event`; it executes in the
kernel without Detcore's prehook, scheduler, logical-time update, statistics, or
unsupported-syscall check. Consequently
`--panic-on-unsupported-syscalls` does not detect the 291 missing release
entries. It only affects an unsupported syscall that some subscriber already
caused Reverie to trap.

## Classification rules

- **DETERMINIZED:** release Detcore subscribes to the syscall and its specific
  handler replaces nondeterministic output/effect or coordinates it through
  Detcore state or the scheduler. The table separately marks complete versus
  partial behavior.
- **PASSTHROUGH:** source contains a deliberate pass-through path with no
  meaningful deterministic model. Thirteen startup/memory passthrough arms are
  not subscribed in release and therefore only run in debug builds or when
  another tool adds the subscription.
- **BLOCKED:** Detcore deliberately refuses the operation by returning
  `ENOSYS` or panicking.
- **MISSING:** no effective optimized-run Detcore coverage. This includes four
  syscalls with dormant dispatch arms but no release subscription:
  `sendto`, `recvmsg`, `sendmsg`, and `sendmmsg`.
- **Release trap:** `always`, conditional on `metadata`,
  `time`, `scheduler`, or bind-analysis policy, or `none`.

The categories describe `hermit run` with Detcore's `NoopTool`. Record and
replay OR a Recorder subscription into Detcore and can capture additional
calls, but recorded output is not a Detcore semantic model and the Recorder is
also selective.

## Source architecture

The task pointed at `detcore/src/tool_global.rs`, but syscall selection and
dispatch do not live there:

- `detcore/src/lib.rs:459` builds the release/debug subscriptions.
- `detcore/src/lib.rs:922` dispatches trapped calls.
- `detcore/src/syscalls/*.rs` implements the per-family handlers.
- `detcore/src/tool_local.rs:100` defines the project's own full, partial, and
  non-determinizable split around `record_or_replay`.
- `detcore/src/tool_global.rs` supplies centralized mechanisms used by those
  handlers: resource scheduling, child registration, futex actions,
  deterministic inode/mtime allocation, logical time, deterministic port
  allocation, and logical alarms.

This distinction matters for review: a function in the global state is not
coverage unless the syscall is subscribed, dispatched, and actually invokes
it.

## Implemented model summary

| Area | Syscalls | Coverage and principal limitation |
| --- | --- | --- |
| Randomness and CPU | `getrandom`, `getcpu`, affinity calls | Random bytes and CPU/node output are synthesized. Affinity is only a CPU0 facade; the setter does not retain requested state. |
| Time and sleep | four clock/time queries, `alarm`, `pause`, `nanosleep`, `clock_nanosleep`, `sched_yield` | Queries use logical time and basic waits use the scheduler. Clock IDs/flags and opt-out modes make sleep handling partial. |
| Threads/processes | clone family, `futex`, `wait4`, exits, exec family, `setsid` | Thread scheduling and blocking are coordinated, but PIDs/TIDs are native. Precise futex handles only WAIT/WAKE bitset families; other operations panic. |
| Files and metadata | open/read/write/close, stat family, directory reads, timestamp calls, FD duplication | Resource and FD models cover common paths. Filesystem contents/errors remain external; metadata replaces only selected fields; many namespace mutations and I/O variants are missing. |
| Descriptor objects | socket creation, pipes, eventfd/signalfd/timerfd/memfd/userfaultfd creation | Creation and FD flags/types are tracked. Most later control operations are not modeled, so `close_range`, timer arming, ioctl, and many descriptor families can desynchronize state. |
| Polling/network | `poll`, `epoll_wait`, accept/connect/recvfrom/bind | Internal blocking is converted to deterministic polling where implemented. Network content is external, `epoll_pwait` forwards, and common sibling APIs are missing. |
| Signals/system identity | three rt-signal calls, `uname`, `sysinfo` | Hermit's reserved signal is protected and selected outputs are rewritten. Physical signal timing and some host-derived fields remain. |

Important partial-model details:

- `mmap` is explicitly described as a "far-from-complete placeholder" and
  simply forwards; writable/shared mappings have no resource lifetime model.
- `fcntl` updates Detcore state only for `F_DUPFD*`; all other commands
  forward.
- `write` does not branch on pipe/socket descriptor type as `read` does.
  Blocking pipe writes can hold the scheduled turn, while physically
  nonblocking socket writes can expose `EAGAIN` instead of entering the
  scheduler retry loop.
- `epoll_pwait` performs an empty scheduler request and then a potentially
  blocking host call. It does not use the nonblocking `epoll_wait` loop.
- `futex` precise/polling modes panic for commands beyond WAIT/WAKE and their
  bitset forms. The timeout conversion computes `tv_sec * 1000 + tv_nsec`
  while naming the value nanoseconds, which appears to undercount seconds by
  six orders of magnitude.
- `sysinfo` synthesizes uptime/load/capacity but derives free RAM from live
  `/proc/<pid>/statm`, so it is not fully reproducible.
- `uname` leaves some fields from the host result.
- `clock_getres` says it reports 10 ms but writes 10,000 ns (10 us), and it
  rejects a NULL result pointer even though Linux permits one.
- `getdents*` sorts entries and virtualizes inode numbers only when metadata
  virtualization is enabled; directory membership still comes from the host
  filesystem.
- Nonzero `bind` bookkeeping inserts the current allocator `next_port` in
  the FD-to-port map instead of the actual bound port, so close-time cleanup
  can remove the wrong reservation.
- `recvfrom` is subscribed, while the otherwise shared handlers for
  `sendto`, `recvmsg`, `sendmsg`, and `sendmmsg` are not.

## Explicit passthrough and blocking

Always-trapped passthroughs:

- `mmap`, `utimensat`, and `epoll_pwait`.
- `add_key`, `request_key`, and `keyctl`; their source TODO calls out
  key-serial virtualization.

Explicit passthrough arms without release subscriptions:

- `brk`, `readlink`, `access`, `mprotect`, `arch_prctl`,
  `set_tid_address`, `set_robust_list`, `prlimit64`, `readlinkat`,
  `madvise`, `munmap`, `prctl`, and `sigaltstack`.

Blocked entries:

- `futimesat` returns `ENOSYS`.
- Deprecated `epoll_ctl_old` and `epoll_wait_old` panic.

## High-impact missing coverage

| Priority | Gap | Why it matters | Recommended initial policy |
| --- | --- | --- | --- |
| P0 | `select`, `pselect6`, `ppoll`, `epoll_pwait2` | Common waits can block while the only scheduled thread owns Detcore's turn. `select` and `pselect6` are already explicit TODOs. | Add timeout/signal-mask-aware nonblocking polling; block in strict mode until modeled. |
| P0 | `futex_waitv` and futex2 `futex_wait`/`wake`/`requeue` | Modern runtimes can bypass the modeled futex scheduler and expose host wake ordering. | Extend the precise futex model or return `ENOSYS` in strict mode. |
| P0 | `io_uring_setup`, `io_uring_enter`, `io_uring_register` | Kernel completion writes shared CQ memory asynchronously, outside later syscall boundaries. Trapping only `enter` is insufficient. | Initially block all three; a real model must own rings, completion order, and registered resources. |
| P0 | Legacy AIO `io_setup` through `io_pgetevents` | Same asynchronous completion and unmanaged-wait problem as io_uring. | Block or record/replay the entire AIO context as one facility. |
| P0 | `close_range`, `openat2`, `pidfd_getfd` | These mutate the FD table without Detcore bookkeeping, causing later handlers to use stale/missing `DetFd` state. | Add FD lifecycle handlers before allowing them. |
| P0 | `recvmmsg`; dormant send/receive dispatch arms | Blocking and timeout semantics bypass scheduler integration; release subscriptions do not match dispatch. | Subscribe all handled siblings and implement `recvmmsg` timeout behavior. |
| P1 | `timerfd_settime`, `timerfd_gettime` | Detcore tracks creation but not logical timer state, so reads are driven by host clocks. | Model timerfd state against logical time. |
| P1 | `readv`/`writev`, positional/vector I/O, `sendfile`, splice family, `copy_file_range` | Short I/O, blocking, cross-FD resource conflicts, and file mutation bypass existing read/write rules. | Generalize the FD/resource and deterministic-I/O helpers. |
| P1 | `getpid`, `gettid`, `getppid`, PID signal calls, pidfds | Native identifiers vary across runs and are already a source TODO in clone handling. | Introduce PID/TID translation consistently across return values, arguments, procfs, wait, and signals. |
| P1 | Filesystem mutations such as rename/link/unlink/mkdir families | Concurrent namespace side effects are not ordered by path/inode resources, and metadata caches are not invalidated. | Add multi-path resource acquisition and inode/FD cache updates. |
| P1 | `rseq` | CPU identity, migration, and abort points expose host scheduling to modern libc/runtime fast paths. | Emulate a stable single-CPU contract or reject registration. |
| P2 | `mremap`, `msync`, shared mapping lifecycle; many ioctls | The existing mmap model is already incomplete and can miss memory/file side effects. | Build mapping/resource tracking; use device-specific ioctl allowlists. |
| P2 | New ABI 451-469 | None has Detcore-specific coverage, including `cachestat`, new futex calls, mount queries, LSM calls, xattr-at calls, and file attribute APIs. | Generate an ABI-drift report in CI and choose model/pass/block explicitly for each addition. |

## Recommended coverage plan

1. Make strict mode fail closed. A release strict run must subscribe to all
   syscalls (or install an equivalent deny filter) before
   `panic_on_unsupported_syscalls` can be trusted. Keep a lower-overhead
   allowlisted mode only as an explicit compatibility choice.
2. Close unmanaged blocking first: select/poll/epoll variants, modern futex,
   `recvmmsg`, and AIO. These can deadlock Hermit in addition to producing
   nondeterminism.
3. Protect the FD model next: `close_range`, `openat2`, pidfd duplication,
   timerfd controls, vector I/O, and cross-FD operations.
4. Block io_uring and legacy AIO until their asynchronous shared-memory effects
   can be modeled or recorded as a facility. Per-syscall wrappers alone do not
   provide determinism.
5. Virtualize PID/TID identity and signal targeting end to end.
6. Generate this inventory from a pinned Linux syscall table in CI. Fail when
   a new syscall lacks an explicit model, passthrough rationale, or block
   policy. Also assert that every Detcore dispatch arm intended for run mode is
   present in the release subscription.

## Complete x86_64 table

The table is sorted by syscall number. The kernel's unassigned/reserved
336-423 range is omitted because it has no named ABI entries in the baseline
header.

| NR | Syscall | Status | Release trap | Determinization | Behavior / limitation |
| ---: | --- | --- | --- | --- | --- |
| 0 | `read` | DETERMINIZED | always | partial | Full-length mode and PRNG-backed random devices; ordinary file/socket data and errors remain host-backed. |
| 1 | `write` | DETERMINIZED | always | partial | Full-length mode plus file resource/mtime ordering; host side effects and errors remain. |
| 2 | `open` | DETERMINIZED | always | partial | Path resource ordering and FD tracking; host path lookup, errors, and contents remain. |
| 3 | `close` | DETERMINIZED | always | partial | FD and deterministic-port bookkeeping around a host close. |
| 4 | `stat` | DETERMINIZED | metadata | partial | Metadata mode rewrites inode and times; other fields and existence/errors remain host-backed; fstat has an explicit FIXME. |
| 5 | `fstat` | DETERMINIZED | metadata | partial | Metadata mode rewrites inode and times; other fields and existence/errors remain host-backed; fstat has an explicit FIXME. |
| 6 | `lstat` | DETERMINIZED | metadata | partial | Metadata mode rewrites inode and times; other fields and existence/errors remain host-backed; fstat has an explicit FIXME. |
| 7 | `poll` | DETERMINIZED | always | partial | Internal FDs use deterministic nonblocking polling; external/record modes use host blocking timing. |
| 8 | `lseek` | MISSING | none | none | No Detcore-specific release coverage. |
| 9 | `mmap` | PASSTHROUGH | always | none | Subscribed placeholder; both branches forward and writable/shared mappings are not resource modeled. |
| 10 | `mprotect` | PASSTHROUGH | none | none | Explicit passthrough arm, but no release subscription; reached only in debug or via an extra subscriber. |
| 11 | `munmap` | PASSTHROUGH | none | none | Explicit passthrough arm, but no release subscription; reached only in debug or via an extra subscriber. |
| 12 | `brk` | PASSTHROUGH | none | none | Explicit passthrough arm, but no release subscription; reached only in debug or via an extra subscriber. |
| 13 | `rt_sigaction` | DETERMINIZED | always | partial | Protects Hermit's reserved signal and/or polls through scheduler; physical signal timing and full signal semantics remain. |
| 14 | `rt_sigprocmask` | DETERMINIZED | always | partial | Protects Hermit's reserved signal and/or polls through scheduler; physical signal timing and full signal semantics remain. |
| 15 | `rt_sigreturn` | MISSING | none | none | No Detcore-specific release coverage. |
| 16 | `ioctl` | MISSING | none | none | No Detcore-specific release coverage. |
| 17 | `pread64` | MISSING | none | none | Offset I/O bypasses deterministic short-I/O and FD resource handling. |
| 18 | `pwrite64` | MISSING | none | none | Offset I/O bypasses deterministic short-I/O and FD resource handling. |
| 19 | `readv` | MISSING | none | none | Vectored I/O bypasses deterministic short-I/O and FD resource handling. |
| 20 | `writev` | MISSING | none | none | Vectored I/O bypasses deterministic short-I/O and FD resource handling. |
| 21 | `access` | PASSTHROUGH | none | none | Explicit passthrough arm, but no release subscription; reached only in debug or via an extra subscriber. |
| 22 | `pipe` | DETERMINIZED | always | partial | Host FD allocation plus Detcore FD-table bookkeeping. |
| 23 | `select` | MISSING | none | none | Explicit TODO in subscriptions; unmanaged blocking wait. |
| 24 | `sched_yield` | DETERMINIZED | always | complete when enabled | Scheduler yield with no host syscall when sequentializing; forwards otherwise. |
| 25 | `mremap` | MISSING | none | none | Address-space mutation is outside the incomplete mmap model. |
| 26 | `msync` | MISSING | none | none | Mapped-file side effects are not resource modeled. |
| 27 | `mincore` | MISSING | none | none | No Detcore-specific release coverage. |
| 28 | `madvise` | PASSTHROUGH | none | none | Explicit passthrough arm, but no release subscription; reached only in debug or via an extra subscriber. |
| 29 | `shmget` | MISSING | none | none | No Detcore-specific release coverage. |
| 30 | `shmat` | MISSING | none | none | No Detcore-specific release coverage. |
| 31 | `shmctl` | MISSING | none | none | No Detcore-specific release coverage. |
| 32 | `dup` | DETERMINIZED | always | partial | Host FD allocation plus Detcore FD-table bookkeeping. |
| 33 | `dup2` | DETERMINIZED | always | partial | Host FD allocation plus Detcore FD-table bookkeeping. |
| 34 | `pause` | DETERMINIZED | scheduler | complete when enabled | Scheduler-owned unbounded sleep when thread scheduling is enabled; host pause otherwise. |
| 35 | `nanosleep` | DETERMINIZED | always | partial | Logical scheduler sleep for supported flags/configurations; unsupported modes and clock fidelity remain host-backed. |
| 36 | `getitimer` | MISSING | none | none | No Detcore-specific release coverage. |
| 37 | `alarm` | DETERMINIZED | scheduler | complete when enabled | Scheduler-owned logical alarm when thread scheduling is enabled; host alarm otherwise. |
| 38 | `setitimer` | MISSING | none | none | No Detcore-specific release coverage. |
| 39 | `getpid` | MISSING | none | none | Native PID is exposed; PID virtualization is a source TODO. |
| 40 | `sendfile` | MISSING | none | none | Cross-FD I/O bypasses resource ordering and short-I/O handling. |
| 41 | `socket` | DETERMINIZED | always | partial | Forces physical nonblocking mode under scheduling and tracks FDs; protocol state remains host-backed. |
| 42 | `connect` | DETERMINIZED | scheduler | partial | Nonblocking retry/scheduler integration; peer timing and network payloads are not deterministic. |
| 43 | `accept` | DETERMINIZED | always | partial | Nonblocking retry/scheduler integration; peer timing and network payloads are not deterministic. |
| 44 | `sendto` | MISSING | none | none | Dispatch helper exists but no release subscription; dormant in optimized run. |
| 45 | `recvfrom` | DETERMINIZED | always | partial | Nonblocking retry/scheduler integration; peer timing and network payloads are not deterministic. |
| 46 | `sendmsg` | MISSING | none | none | Dispatch helper exists but no release subscription; dormant in optimized run. |
| 47 | `recvmsg` | MISSING | none | none | Dispatch helper exists but no release subscription; dormant in optimized run. |
| 48 | `shutdown` | MISSING | none | none | No Detcore-specific release coverage. |
| 49 | `bind` | DETERMINIZED | scheduler / bind policy | partial | Port 0 is assigned deterministically for AF_INET/AF_INET6; other families and host port state remain. |
| 50 | `listen` | MISSING | none | none | No Detcore-specific release coverage. |
| 51 | `getsockname` | MISSING | none | none | No Detcore-specific release coverage. |
| 52 | `getpeername` | MISSING | none | none | No Detcore-specific release coverage. |
| 53 | `socketpair` | DETERMINIZED | always | partial | Forces physical nonblocking mode under scheduling and tracks FDs; protocol state remains host-backed. |
| 54 | `setsockopt` | MISSING | none | none | No Detcore-specific release coverage. |
| 55 | `getsockopt` | MISSING | none | none | No Detcore-specific release coverage. |
| 56 | `clone` | DETERMINIZED | always | partial | Scheduler/thread registration and vfork coordination; native PIDs/TIDs and kernel clone semantics remain. |
| 57 | `fork` | DETERMINIZED | always | partial | Scheduler/thread registration and vfork coordination; native PIDs/TIDs and kernel clone semantics remain. |
| 58 | `vfork` | DETERMINIZED | always | partial | Scheduler/thread registration and vfork coordination; native PIDs/TIDs and kernel clone semantics remain. |
| 59 | `execve` | DETERMINIZED | always | partial | CLOEXEC FD bookkeeping; image lookup, errors, and native process identity remain host-backed. |
| 60 | `exit` | DETERMINIZED | always | partial | Scheduler-coordinated exit followed by a kernel tail call. |
| 61 | `wait4` | DETERMINIZED | always | partial | Scheduler-managed nonblocking polling; child IDs, status, and errors remain kernel-provided. |
| 62 | `kill` | MISSING | none | none | Signal target identity/delivery is not virtualized. |
| 63 | `uname` | DETERMINIZED | always | partial | Rewrites node/domain/release/version; remaining fields derive from the host result. |
| 64 | `semget` | MISSING | none | none | No Detcore-specific release coverage. |
| 65 | `semop` | MISSING | none | none | No Detcore-specific release coverage. |
| 66 | `semctl` | MISSING | none | none | No Detcore-specific release coverage. |
| 67 | `shmdt` | MISSING | none | none | No Detcore-specific release coverage. |
| 68 | `msgget` | MISSING | none | none | No Detcore-specific release coverage. |
| 69 | `msgsnd` | MISSING | none | none | No Detcore-specific release coverage. |
| 70 | `msgrcv` | MISSING | none | none | No Detcore-specific release coverage. |
| 71 | `msgctl` | MISSING | none | none | No Detcore-specific release coverage. |
| 72 | `fcntl` | DETERMINIZED | always | partial | Only F_DUPFD and F_DUPFD_CLOEXEC update the FD model; every other command forwards. |
| 73 | `flock` | MISSING | none | none | No Detcore-specific release coverage. |
| 74 | `fsync` | MISSING | none | none | No Detcore-specific release coverage. |
| 75 | `fdatasync` | MISSING | none | none | No Detcore-specific release coverage. |
| 76 | `truncate` | MISSING | none | none | No Detcore-specific release coverage. |
| 77 | `ftruncate` | MISSING | none | none | No Detcore-specific release coverage. |
| 78 | `getdents` | DETERMINIZED | metadata | partial | Metadata mode sorts entries and virtualizes inode numbers; directory contents still depend on the filesystem. |
| 79 | `getcwd` | MISSING | none | none | No Detcore-specific release coverage. |
| 80 | `chdir` | MISSING | none | none | No Detcore-specific release coverage. |
| 81 | `fchdir` | MISSING | none | none | No Detcore-specific release coverage. |
| 82 | `rename` | MISSING | none | none | Filesystem namespace mutation has no Detcore resource handler. |
| 83 | `mkdir` | MISSING | none | none | Filesystem namespace mutation has no Detcore resource handler. |
| 84 | `rmdir` | MISSING | none | none | No Detcore-specific release coverage. |
| 85 | `creat` | DETERMINIZED | always | partial | Path resource ordering and FD tracking; host path lookup, errors, and contents remain. |
| 86 | `link` | MISSING | none | none | Filesystem namespace mutation has no Detcore resource handler. |
| 87 | `unlink` | MISSING | none | none | Filesystem namespace mutation has no Detcore resource handler. |
| 88 | `symlink` | MISSING | none | none | No Detcore-specific release coverage. |
| 89 | `readlink` | PASSTHROUGH | none | none | Explicit passthrough arm, but no release subscription; reached only in debug or via an extra subscriber. |
| 90 | `chmod` | MISSING | none | none | No Detcore-specific release coverage. |
| 91 | `fchmod` | MISSING | none | none | No Detcore-specific release coverage. |
| 92 | `chown` | MISSING | none | none | No Detcore-specific release coverage. |
| 93 | `fchown` | MISSING | none | none | No Detcore-specific release coverage. |
| 94 | `lchown` | MISSING | none | none | No Detcore-specific release coverage. |
| 95 | `umask` | MISSING | none | none | No Detcore-specific release coverage. |
| 96 | `gettimeofday` | DETERMINIZED | time | partial | Logical timeval overwrites the host result when enabled; legacy timezone behavior is not modeled. |
| 97 | `getrlimit` | MISSING | none | none | No Detcore-specific release coverage. |
| 98 | `getrusage` | MISSING | none | none | No Detcore-specific release coverage. |
| 99 | `sysinfo` | DETERMINIZED | always | partial | Logical uptime and constants, but free RAM derives from live /proc resident pages. |
| 100 | `times` | MISSING | none | none | No Detcore-specific release coverage. |
| 101 | `ptrace` | MISSING | none | none | No Detcore-specific release coverage. |
| 102 | `getuid` | MISSING | none | none | No Detcore-specific release coverage. |
| 103 | `syslog` | MISSING | none | none | No Detcore-specific release coverage. |
| 104 | `getgid` | MISSING | none | none | No Detcore-specific release coverage. |
| 105 | `setuid` | MISSING | none | none | No Detcore-specific release coverage. |
| 106 | `setgid` | MISSING | none | none | No Detcore-specific release coverage. |
| 107 | `geteuid` | MISSING | none | none | No Detcore-specific release coverage. |
| 108 | `getegid` | MISSING | none | none | No Detcore-specific release coverage. |
| 109 | `setpgid` | MISSING | none | none | No Detcore-specific release coverage. |
| 110 | `getppid` | MISSING | none | none | Native parent PID is exposed. |
| 111 | `getpgrp` | MISSING | none | none | No Detcore-specific release coverage. |
| 112 | `setsid` | DETERMINIZED | always | partial | Host call plus daemon lifecycle tracking; native session/PID values remain. |
| 113 | `setreuid` | MISSING | none | none | No Detcore-specific release coverage. |
| 114 | `setregid` | MISSING | none | none | No Detcore-specific release coverage. |
| 115 | `getgroups` | MISSING | none | none | No Detcore-specific release coverage. |
| 116 | `setgroups` | MISSING | none | none | No Detcore-specific release coverage. |
| 117 | `setresuid` | MISSING | none | none | No Detcore-specific release coverage. |
| 118 | `getresuid` | MISSING | none | none | No Detcore-specific release coverage. |
| 119 | `setresgid` | MISSING | none | none | No Detcore-specific release coverage. |
| 120 | `getresgid` | MISSING | none | none | No Detcore-specific release coverage. |
| 121 | `getpgid` | MISSING | none | none | No Detcore-specific release coverage. |
| 122 | `setfsuid` | MISSING | none | none | No Detcore-specific release coverage. |
| 123 | `setfsgid` | MISSING | none | none | No Detcore-specific release coverage. |
| 124 | `getsid` | MISSING | none | none | No Detcore-specific release coverage. |
| 125 | `capget` | MISSING | none | none | No Detcore-specific release coverage. |
| 126 | `capset` | MISSING | none | none | No Detcore-specific release coverage. |
| 127 | `rt_sigpending` | MISSING | none | none | No Detcore-specific release coverage. |
| 128 | `rt_sigtimedwait` | DETERMINIZED | always | partial | Protects Hermit's reserved signal and/or polls through scheduler; physical signal timing and full signal semantics remain. |
| 129 | `rt_sigqueueinfo` | MISSING | none | none | No Detcore-specific release coverage. |
| 130 | `rt_sigsuspend` | MISSING | none | none | No Detcore-specific release coverage. |
| 131 | `sigaltstack` | PASSTHROUGH | none | none | Explicit passthrough arm, but no release subscription; reached only in debug or via an extra subscriber. |
| 132 | `utime` | DETERMINIZED | always | partial | NULL timestamp inputs use logical time, then the update reaches the host filesystem. |
| 133 | `mknod` | MISSING | none | none | No Detcore-specific release coverage. |
| 134 | `uselib` | MISSING | none | none | No Detcore-specific release coverage. |
| 135 | `personality` | MISSING | none | none | No Detcore-specific release coverage. |
| 136 | `ustat` | MISSING | none | none | No Detcore-specific release coverage. |
| 137 | `statfs` | MISSING | none | none | No Detcore-specific release coverage. |
| 138 | `fstatfs` | MISSING | none | none | No Detcore-specific release coverage. |
| 139 | `sysfs` | MISSING | none | none | No Detcore-specific release coverage. |
| 140 | `getpriority` | MISSING | none | none | No Detcore-specific release coverage. |
| 141 | `setpriority` | MISSING | none | none | No Detcore-specific release coverage. |
| 142 | `sched_setparam` | MISSING | none | none | No Detcore-specific release coverage. |
| 143 | `sched_getparam` | MISSING | none | none | No Detcore-specific release coverage. |
| 144 | `sched_setscheduler` | MISSING | none | none | No Detcore-specific release coverage. |
| 145 | `sched_getscheduler` | MISSING | none | none | No Detcore-specific release coverage. |
| 146 | `sched_get_priority_max` | MISSING | none | none | No Detcore-specific release coverage. |
| 147 | `sched_get_priority_min` | MISSING | none | none | No Detcore-specific release coverage. |
| 148 | `sched_rr_get_interval` | MISSING | none | none | No Detcore-specific release coverage. |
| 149 | `mlock` | MISSING | none | none | No Detcore-specific release coverage. |
| 150 | `munlock` | MISSING | none | none | No Detcore-specific release coverage. |
| 151 | `mlockall` | MISSING | none | none | No Detcore-specific release coverage. |
| 152 | `munlockall` | MISSING | none | none | No Detcore-specific release coverage. |
| 153 | `vhangup` | MISSING | none | none | No Detcore-specific release coverage. |
| 154 | `modify_ldt` | MISSING | none | none | No Detcore-specific release coverage. |
| 155 | `pivot_root` | MISSING | none | none | No Detcore-specific release coverage. |
| 156 | `_sysctl` | MISSING | none | none | No Detcore-specific release coverage. |
| 157 | `prctl` | PASSTHROUGH | none | none | Explicit passthrough arm, but no release subscription; reached only in debug or via an extra subscriber. |
| 158 | `arch_prctl` | PASSTHROUGH | none | none | Explicit passthrough arm, but no release subscription; reached only in debug or via an extra subscriber. |
| 159 | `adjtimex` | MISSING | none | none | No Detcore-specific release coverage. |
| 160 | `setrlimit` | MISSING | none | none | No Detcore-specific release coverage. |
| 161 | `chroot` | MISSING | none | none | No Detcore-specific release coverage. |
| 162 | `sync` | MISSING | none | none | No Detcore-specific release coverage. |
| 163 | `acct` | MISSING | none | none | No Detcore-specific release coverage. |
| 164 | `settimeofday` | MISSING | none | none | No Detcore-specific release coverage. |
| 165 | `mount` | MISSING | none | none | No Detcore-specific release coverage. |
| 166 | `umount2` | MISSING | none | none | No Detcore-specific release coverage. |
| 167 | `swapon` | MISSING | none | none | No Detcore-specific release coverage. |
| 168 | `swapoff` | MISSING | none | none | No Detcore-specific release coverage. |
| 169 | `reboot` | MISSING | none | none | No Detcore-specific release coverage. |
| 170 | `sethostname` | MISSING | none | none | No Detcore-specific release coverage. |
| 171 | `setdomainname` | MISSING | none | none | No Detcore-specific release coverage. |
| 172 | `iopl` | MISSING | none | none | No Detcore-specific release coverage. |
| 173 | `ioperm` | MISSING | none | none | No Detcore-specific release coverage. |
| 174 | `create_module` | MISSING | none | none | No Detcore-specific release coverage. |
| 175 | `init_module` | MISSING | none | none | No Detcore-specific release coverage. |
| 176 | `delete_module` | MISSING | none | none | No Detcore-specific release coverage. |
| 177 | `get_kernel_syms` | MISSING | none | none | No Detcore-specific release coverage. |
| 178 | `query_module` | MISSING | none | none | No Detcore-specific release coverage. |
| 179 | `quotactl` | MISSING | none | none | No Detcore-specific release coverage. |
| 180 | `nfsservctl` | MISSING | none | none | No Detcore-specific release coverage. |
| 181 | `getpmsg` | MISSING | none | none | No Detcore-specific release coverage. |
| 182 | `putpmsg` | MISSING | none | none | No Detcore-specific release coverage. |
| 183 | `afs_syscall` | MISSING | none | none | No Detcore-specific release coverage. |
| 184 | `tuxcall` | MISSING | none | none | No Detcore-specific release coverage. |
| 185 | `security` | MISSING | none | none | No Detcore-specific release coverage. |
| 186 | `gettid` | MISSING | none | none | Native TID is exposed; thread IDs vary across runs. |
| 187 | `readahead` | MISSING | none | none | No Detcore-specific release coverage. |
| 188 | `setxattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 189 | `lsetxattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 190 | `fsetxattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 191 | `getxattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 192 | `lgetxattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 193 | `fgetxattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 194 | `listxattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 195 | `llistxattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 196 | `flistxattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 197 | `removexattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 198 | `lremovexattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 199 | `fremovexattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 200 | `tkill` | MISSING | none | none | Native TID signal targeting bypasses the scheduler model. |
| 201 | `time` | DETERMINIZED | time | complete when enabled | Logical seconds synthesized when time virtualization is enabled. |
| 202 | `futex` | DETERMINIZED | always | partial | Precise mode emulates WAIT/WAKE and bitset variants only; other ops panic, modes differ, and seconds-to-ns conversion appears incorrect. |
| 203 | `sched_setaffinity` | DETERMINIZED | always | partial | Stable CPU0 view/no-op setter, but requested affinity is not retained and Linux fidelity is incomplete. |
| 204 | `sched_getaffinity` | DETERMINIZED | always | partial | Stable CPU0 view/no-op setter, but requested affinity is not retained and Linux fidelity is incomplete. |
| 205 | `set_thread_area` | MISSING | none | none | No Detcore-specific release coverage. |
| 206 | `io_setup` | MISSING | none | none | Legacy kernel AIO is unmodeled and can complete asynchronously. |
| 207 | `io_destroy` | MISSING | none | none | Legacy kernel AIO lifecycle is unmodeled. |
| 208 | `io_getevents` | MISSING | none | none | Legacy kernel AIO completion wait is unmanaged. |
| 209 | `io_submit` | MISSING | none | none | Legacy kernel AIO submission is unmodeled. |
| 210 | `io_cancel` | MISSING | none | none | Legacy kernel AIO cancellation is unmodeled. |
| 211 | `get_thread_area` | MISSING | none | none | No Detcore-specific release coverage. |
| 212 | `lookup_dcookie` | MISSING | none | none | No Detcore-specific release coverage. |
| 213 | `epoll_create` | DETERMINIZED | always | partial | Scheduler checkpoint plus kernel epoll state; event object semantics are not modeled. |
| 214 | `epoll_ctl_old` | BLOCKED | always | none | Panics when called; deprecated ABI is deliberately unsupported. |
| 215 | `epoll_wait_old` | BLOCKED | always | none | Panics when called; deprecated ABI is deliberately unsupported. |
| 216 | `remap_file_pages` | MISSING | none | none | No Detcore-specific release coverage. |
| 217 | `getdents64` | DETERMINIZED | metadata | partial | Metadata mode sorts entries and virtualizes inode numbers; directory contents still depend on the filesystem. |
| 218 | `set_tid_address` | PASSTHROUGH | none | none | Explicit passthrough arm, but no release subscription; reached only in debug or via an extra subscriber. |
| 219 | `restart_syscall` | MISSING | none | none | No Detcore-specific release coverage. |
| 220 | `semtimedop` | MISSING | none | none | No Detcore-specific release coverage. |
| 221 | `fadvise64` | MISSING | none | none | No Detcore-specific release coverage. |
| 222 | `timer_create` | MISSING | none | none | No Detcore-specific release coverage. |
| 223 | `timer_settime` | MISSING | none | none | No Detcore-specific release coverage. |
| 224 | `timer_gettime` | MISSING | none | none | No Detcore-specific release coverage. |
| 225 | `timer_getoverrun` | MISSING | none | none | No Detcore-specific release coverage. |
| 226 | `timer_delete` | MISSING | none | none | No Detcore-specific release coverage. |
| 227 | `clock_settime` | MISSING | none | none | No Detcore-specific release coverage. |
| 228 | `clock_gettime` | DETERMINIZED | time | complete when enabled | Logical time synthesized when enabled; clock IDs share one logical domain. |
| 229 | `clock_getres` | DETERMINIZED | time | complete when enabled | Fixed deterministic resolution when enabled; NULL/output fidelity is limited. |
| 230 | `clock_nanosleep` | DETERMINIZED | always | partial | Logical scheduler sleep for supported flags/configurations; unsupported modes and clock fidelity remain host-backed. |
| 231 | `exit_group` | DETERMINIZED | always | partial | Scheduler-coordinated exit followed by a kernel tail call. |
| 232 | `epoll_wait` | DETERMINIZED | always | partial | Internal waits use deterministic nonblocking polling; external/record modes use host blocking timing. |
| 233 | `epoll_ctl` | DETERMINIZED | always | partial | Scheduler checkpoint plus kernel epoll state; event object semantics are not modeled. |
| 234 | `tgkill` | MISSING | none | none | Native PID/TID signal targeting bypasses the scheduler model. |
| 235 | `utimes` | DETERMINIZED | always | partial | NULL timestamp inputs use logical time, then the update reaches the host filesystem. |
| 236 | `vserver` | MISSING | none | none | No Detcore-specific release coverage. |
| 237 | `mbind` | MISSING | none | none | No Detcore-specific release coverage. |
| 238 | `set_mempolicy` | MISSING | none | none | No Detcore-specific release coverage. |
| 239 | `get_mempolicy` | MISSING | none | none | No Detcore-specific release coverage. |
| 240 | `mq_open` | MISSING | none | none | No Detcore-specific release coverage. |
| 241 | `mq_unlink` | MISSING | none | none | No Detcore-specific release coverage. |
| 242 | `mq_timedsend` | MISSING | none | none | No Detcore-specific release coverage. |
| 243 | `mq_timedreceive` | MISSING | none | none | No Detcore-specific release coverage. |
| 244 | `mq_notify` | MISSING | none | none | No Detcore-specific release coverage. |
| 245 | `mq_getsetattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 246 | `kexec_load` | MISSING | none | none | No Detcore-specific release coverage. |
| 247 | `waitid` | MISSING | none | none | No Detcore-specific release coverage. |
| 248 | `add_key` | PASSTHROUGH | always | none | Always trapped, then explicitly forwarded; key serial-number virtualization is TODO. |
| 249 | `request_key` | PASSTHROUGH | always | none | Always trapped, then explicitly forwarded; key serial-number virtualization is TODO. |
| 250 | `keyctl` | PASSTHROUGH | always | none | Always trapped, then explicitly forwarded; key serial-number virtualization is TODO. |
| 251 | `ioprio_set` | MISSING | none | none | No Detcore-specific release coverage. |
| 252 | `ioprio_get` | MISSING | none | none | No Detcore-specific release coverage. |
| 253 | `inotify_init` | MISSING | none | none | No Detcore-specific release coverage. |
| 254 | `inotify_add_watch` | MISSING | none | none | No Detcore-specific release coverage. |
| 255 | `inotify_rm_watch` | MISSING | none | none | No Detcore-specific release coverage. |
| 256 | `migrate_pages` | MISSING | none | none | No Detcore-specific release coverage. |
| 257 | `openat` | DETERMINIZED | always | partial | Path resource ordering and FD tracking; host path lookup, errors, and contents remain. |
| 258 | `mkdirat` | MISSING | none | none | No Detcore-specific release coverage. |
| 259 | `mknodat` | MISSING | none | none | No Detcore-specific release coverage. |
| 260 | `fchownat` | MISSING | none | none | No Detcore-specific release coverage. |
| 261 | `futimesat` | BLOCKED | always | none | Returns ENOSYS unconditionally. |
| 262 | `newfstatat` | DETERMINIZED | metadata | partial | Metadata mode rewrites inode and times; other fields and existence/errors remain host-backed; fstat has an explicit FIXME. |
| 263 | `unlinkat` | MISSING | none | none | Filesystem namespace mutation has no Detcore resource handler. |
| 264 | `renameat` | MISSING | none | none | Filesystem namespace mutation has no Detcore resource handler. |
| 265 | `linkat` | MISSING | none | none | Filesystem namespace mutation has no Detcore resource handler. |
| 266 | `symlinkat` | MISSING | none | none | No Detcore-specific release coverage. |
| 267 | `readlinkat` | PASSTHROUGH | none | none | Explicit passthrough arm, but no release subscription; reached only in debug or via an extra subscriber. |
| 268 | `fchmodat` | MISSING | none | none | No Detcore-specific release coverage. |
| 269 | `faccessat` | MISSING | none | none | No Detcore-specific release coverage. |
| 270 | `pselect6` | MISSING | none | none | Explicit TODO in subscriptions; unmanaged blocking wait. |
| 271 | `ppoll` | MISSING | none | none | No Detcore handler; unmanaged blocking wait. |
| 272 | `unshare` | MISSING | none | none | No Detcore-specific release coverage. |
| 273 | `set_robust_list` | PASSTHROUGH | none | none | Explicit passthrough arm, but no release subscription; reached only in debug or via an extra subscriber. |
| 274 | `get_robust_list` | MISSING | none | none | No Detcore-specific release coverage. |
| 275 | `splice` | MISSING | none | none | Cross-FD I/O bypasses resource ordering and blocking handling. |
| 276 | `tee` | MISSING | none | none | Cross-FD pipe I/O bypasses resource ordering and blocking handling. |
| 277 | `sync_file_range` | MISSING | none | none | No Detcore-specific release coverage. |
| 278 | `vmsplice` | MISSING | none | none | Pipe I/O bypasses resource ordering and blocking handling. |
| 279 | `move_pages` | MISSING | none | none | No Detcore-specific release coverage. |
| 280 | `utimensat` | PASSTHROUGH | always | none | Subscribed handler directly forwards to record/replay or the host kernel. |
| 281 | `epoll_pwait` | PASSTHROUGH | always | none | Empty scheduler checkpoint, then host call; blocking, timeout, and signal-mask behavior are not determinized. |
| 282 | `signalfd` | DETERMINIZED | always | partial | Tracks descriptor type/flags only; most subsequent object operations remain missing or host-backed. |
| 283 | `timerfd_create` | DETERMINIZED | always | partial | Tracks descriptor type/flags only; most subsequent object operations remain missing or host-backed. |
| 284 | `eventfd` | DETERMINIZED | always | partial | Tracks descriptor type/flags only; most subsequent object operations remain missing or host-backed. |
| 285 | `fallocate` | MISSING | none | none | No Detcore-specific release coverage. |
| 286 | `timerfd_settime` | MISSING | none | none | timerfd_create is tracked, but timer arming remains host-time behavior. |
| 287 | `timerfd_gettime` | MISSING | none | none | timerfd_create is tracked, but timer state remains host-time behavior. |
| 288 | `accept4` | DETERMINIZED | always | partial | Nonblocking retry/scheduler integration; peer timing and network payloads are not deterministic. |
| 289 | `signalfd4` | DETERMINIZED | always | partial | Tracks descriptor type/flags only; most subsequent object operations remain missing or host-backed. |
| 290 | `eventfd2` | DETERMINIZED | always | partial | Tracks descriptor type/flags only; most subsequent object operations remain missing or host-backed. |
| 291 | `epoll_create1` | DETERMINIZED | always | partial | Scheduler checkpoint plus kernel epoll state; event object semantics are not modeled. |
| 292 | `dup3` | DETERMINIZED | always | partial | Host FD allocation plus Detcore FD-table bookkeeping. |
| 293 | `pipe2` | DETERMINIZED | always | partial | Host FD allocation plus Detcore FD-table bookkeeping. |
| 294 | `inotify_init1` | MISSING | none | none | No Detcore-specific release coverage. |
| 295 | `preadv` | MISSING | none | none | Vectored offset I/O bypasses deterministic short-I/O and FD resource handling. |
| 296 | `pwritev` | MISSING | none | none | Vectored offset I/O bypasses deterministic short-I/O and FD resource handling. |
| 297 | `rt_tgsigqueueinfo` | MISSING | none | none | No Detcore-specific release coverage. |
| 298 | `perf_event_open` | MISSING | none | none | Guest PMU activity is not modeled and may conflict with instrumentation. |
| 299 | `recvmmsg` | MISSING | none | none | Handler is commented out; timeout behavior is an explicit TODO. |
| 300 | `fanotify_init` | MISSING | none | none | No Detcore-specific release coverage. |
| 301 | `fanotify_mark` | MISSING | none | none | No Detcore-specific release coverage. |
| 302 | `prlimit64` | PASSTHROUGH | none | none | Explicit passthrough arm, but no release subscription; reached only in debug or via an extra subscriber. |
| 303 | `name_to_handle_at` | MISSING | none | none | No Detcore-specific release coverage. |
| 304 | `open_by_handle_at` | MISSING | none | none | No Detcore-specific release coverage. |
| 305 | `clock_adjtime` | MISSING | none | none | No Detcore-specific release coverage. |
| 306 | `syncfs` | MISSING | none | none | No Detcore-specific release coverage. |
| 307 | `sendmmsg` | MISSING | none | none | Dispatch helper exists but no release subscription; dormant in optimized run. |
| 308 | `setns` | MISSING | none | none | No Detcore-specific release coverage. |
| 309 | `getcpu` | DETERMINIZED | always | complete | Synthesizes CPU and NUMA node 0 without a host syscall. |
| 310 | `process_vm_readv` | MISSING | none | none | No Detcore-specific release coverage. |
| 311 | `process_vm_writev` | MISSING | none | none | No Detcore-specific release coverage. |
| 312 | `kcmp` | MISSING | none | none | No Detcore-specific release coverage. |
| 313 | `finit_module` | MISSING | none | none | No Detcore-specific release coverage. |
| 314 | `sched_setattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 315 | `sched_getattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 316 | `renameat2` | MISSING | none | none | Filesystem namespace mutation has no Detcore resource handler. |
| 317 | `seccomp` | MISSING | none | none | Guest filter changes are not modeled by Detcore. |
| 318 | `getrandom` | DETERMINIZED | always | complete | PRNG-backed bytes; all output is reproducible, although Linux flag fidelity is limited. |
| 319 | `memfd_create` | DETERMINIZED | always | partial | Tracks descriptor type/flags only; most subsequent object operations remain missing or host-backed. |
| 320 | `kexec_file_load` | MISSING | none | none | No Detcore-specific release coverage. |
| 321 | `bpf` | MISSING | none | none | No Detcore-specific release coverage. |
| 322 | `execveat` | DETERMINIZED | always | partial | CLOEXEC FD bookkeeping; image lookup, errors, and native process identity remain host-backed. |
| 323 | `userfaultfd` | DETERMINIZED | always | partial | Tracks descriptor type/flags only; most subsequent object operations remain missing or host-backed. |
| 324 | `membarrier` | MISSING | none | none | No Detcore-specific release coverage. |
| 325 | `mlock2` | MISSING | none | none | No Detcore-specific release coverage. |
| 326 | `copy_file_range` | MISSING | none | none | Cross-FD file mutation bypasses resource ordering. |
| 327 | `preadv2` | MISSING | none | none | Vectored offset I/O bypasses deterministic short-I/O and FD resource handling. |
| 328 | `pwritev2` | MISSING | none | none | Vectored offset I/O bypasses deterministic short-I/O and FD resource handling. |
| 329 | `pkey_mprotect` | MISSING | none | none | No Detcore-specific release coverage. |
| 330 | `pkey_alloc` | MISSING | none | none | No Detcore-specific release coverage. |
| 331 | `pkey_free` | MISSING | none | none | No Detcore-specific release coverage. |
| 332 | `statx` | DETERMINIZED | metadata | partial | Metadata mode rewrites inode and times; other fields and existence/errors remain host-backed; fstat has an explicit FIXME. |
| 333 | `io_pgetevents` | MISSING | none | none | Legacy kernel AIO completion wait is unmanaged. |
| 334 | `rseq` | MISSING | none | none | Per-thread restartable-sequence CPU identity and abort behavior are host-dependent. |
| 335 | `uretprobe` | MISSING | none | none | No Detcore-specific release coverage. |
| 424 | `pidfd_send_signal` | MISSING | none | none | PID/signal and pidfd state are not virtualized. |
| 425 | `io_uring_setup` | MISSING | none | none | Async shared-ring I/O is wholly unmodeled; completion writes bypass syscall boundaries. |
| 426 | `io_uring_enter` | MISSING | none | none | Async submission/completion wait is unmanaged. |
| 427 | `io_uring_register` | MISSING | none | none | Async ring resource registration is unmodeled. |
| 428 | `open_tree` | MISSING | none | none | No Detcore-specific release coverage. |
| 429 | `move_mount` | MISSING | none | none | No Detcore-specific release coverage. |
| 430 | `fsopen` | MISSING | none | none | No Detcore-specific release coverage. |
| 431 | `fsconfig` | MISSING | none | none | No Detcore-specific release coverage. |
| 432 | `fsmount` | MISSING | none | none | No Detcore-specific release coverage. |
| 433 | `fspick` | MISSING | none | none | No Detcore-specific release coverage. |
| 434 | `pidfd_open` | MISSING | none | none | Native PID identity and pidfd lifecycle are not virtualized. |
| 435 | `clone3` | DETERMINIZED | always | partial | Scheduler/thread registration and vfork coordination; native PIDs/TIDs and kernel clone semantics remain. |
| 436 | `close_range` | MISSING | none | none | Bulk close bypasses Detcore FD/port bookkeeping. |
| 437 | `openat2` | MISSING | none | none | Path open bypasses path resource ordering and FD tracking. |
| 438 | `pidfd_getfd` | MISSING | none | none | FD duplication bypasses Detcore FD bookkeeping. |
| 439 | `faccessat2` | MISSING | none | none | No Detcore-specific release coverage. |
| 440 | `process_madvise` | MISSING | none | none | No Detcore-specific release coverage. |
| 441 | `epoll_pwait2` | MISSING | none | none | Modern nanosecond epoll wait is unmanaged and can block the scheduled thread. |
| 442 | `mount_setattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 443 | `quotactl_fd` | MISSING | none | none | No Detcore-specific release coverage. |
| 444 | `landlock_create_ruleset` | MISSING | none | none | No Detcore-specific release coverage. |
| 445 | `landlock_add_rule` | MISSING | none | none | No Detcore-specific release coverage. |
| 446 | `landlock_restrict_self` | MISSING | none | none | No Detcore-specific release coverage. |
| 447 | `memfd_secret` | MISSING | none | none | No Detcore-specific release coverage. |
| 448 | `process_mrelease` | MISSING | none | none | No Detcore-specific release coverage. |
| 449 | `futex_waitv` | MISSING | none | none | Modern multi-address futex wait bypasses the scheduler. |
| 450 | `set_mempolicy_home_node` | MISSING | none | none | No Detcore-specific release coverage. |
| 451 | `cachestat` | MISSING | none | none | No Detcore-specific release coverage. |
| 452 | `fchmodat2` | MISSING | none | none | No Detcore-specific release coverage. |
| 453 | `map_shadow_stack` | MISSING | none | none | No Detcore-specific release coverage. |
| 454 | `futex_wake` | MISSING | none | none | futex2 wake operation is not part of the modeled futex family. |
| 455 | `futex_wait` | MISSING | none | none | futex2 wait operation bypasses the scheduler. |
| 456 | `futex_requeue` | MISSING | none | none | futex2 requeue operation is not modeled. |
| 457 | `statmount` | MISSING | none | none | No Detcore-specific release coverage. |
| 458 | `listmount` | MISSING | none | none | No Detcore-specific release coverage. |
| 459 | `lsm_get_self_attr` | MISSING | none | none | No Detcore-specific release coverage. |
| 460 | `lsm_set_self_attr` | MISSING | none | none | No Detcore-specific release coverage. |
| 461 | `lsm_list_modules` | MISSING | none | none | No Detcore-specific release coverage. |
| 462 | `mseal` | MISSING | none | none | No Detcore-specific release coverage. |
| 463 | `setxattrat` | MISSING | none | none | No Detcore-specific release coverage. |
| 464 | `getxattrat` | MISSING | none | none | No Detcore-specific release coverage. |
| 465 | `listxattrat` | MISSING | none | none | No Detcore-specific release coverage. |
| 466 | `removexattrat` | MISSING | none | none | No Detcore-specific release coverage. |
| 467 | `open_tree_attr` | MISSING | none | none | No Detcore-specific release coverage. |
| 468 | `file_getattr` | MISSING | none | none | No Detcore-specific release coverage. |
| 469 | `file_setattr` | MISSING | none | none | No Detcore-specific release coverage. |

## Method and sources

The audit read the current subscription, dispatch, and all handler modules,
then compared their effective optimized-run reach with every name parsed from
`/usr/include/asm/unistd_64.h`. It treated comments as non-code: the
`select` and `pselect6` references do not count as subscriptions, and the
commented `recvmmsg` match arm does not count as a handler.

Primary source locations:

- `detcore/src/lib.rs:459-584`: subscriptions.
- `detcore/src/lib.rs:922-1120`: dispatch and unsupported fallback.
- `detcore/src/tool_local.rs:100-130`: full/partial/non-determinizable model.
- `detcore/src/syscalls/files.rs`, `io.rs`, `misc.rs`, `signal.rs`,
  `sysinfo.rs`, `threads.rs`, and `time.rs`: syscall semantics.
- `detcore/src/tool_global.rs:450-540,1207-1650`: shared scheduler, inode,
  logical-time, port, child, futex, and alarm mechanisms.
- `hermit-cli/src/recorder/mod.rs:77-133`: additional record/replay
  subscriptions.
- `hermit-cli/src/recorder/network.rs:114`: recorder TODO for ppoll, epoll,
  and select.
