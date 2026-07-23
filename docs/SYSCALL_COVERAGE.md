# Hermit Syscall Coverage

This audit describes syscall coverage for normal optimized `hermit run` on
x86_64. It is source-derived from `rrnewton/hermit` commit `6c3854f` and the
373 syscall names in the pinned `syscalls` 0.6.18 x86_64 table. It assesses
Detcore policy, not whether a particular host kernel implements every table
entry.

The headline result is:

| Disposition in a default optimized run | Count | Share |
| --- | ---: | ---: |
| Trapped by Reverie and dispatched through Detcore | 86 | 23.1% |
| Allowed directly to Linux without a Detcore syscall event | 287 | 76.9% |
| Total x86_64 names in the pinned table | 373 | 100% |

This is not the same as saying that 86 syscalls are fully deterministic. Some
trapped handlers only order an injected Linux call, rewrite selected output
fields, maintain Detcore bookkeeping, or explicitly pass the call through.
Conversely, a direct syscall can repeat when all of its inputs and host state
are frozen, but Hermit does not enforce that property.

## Terminology And Scope

- **Emulated** means Detcore returns a result without executing the guest's
  original syscall.
- **Normalized** means Linux executes the operation and Detcore rewrites or
  controls part of the observable result.
- **Ordered/observed** means Detcore schedules the operation or updates a model,
  but Linux still supplies guest-visible state.
- **Trapped passthrough** means Detcore sees the event and injects the call with
  no determinization of its result.
- **Direct passthrough** means release-mode seccomp does not trap the call at
  all. Detcore's common hooks, logical syscall counter, scheduler check-in,
  diagnostics, and `--panic-on-unsupported-syscalls` do not run.

Debug builds use `Subscription::all()`, so unsupported calls reach Detcore's
fallback. That makes debug coverage materially different from optimized
coverage and can hide a missing release subscription. Reverie must always
allow `restart_syscall` and `rt_sigreturn` for its own signal/syscall protocol.

Normal `hermit run` enables serialized scheduling, deterministic I/O, virtual
time, and virtual metadata. Disabling those options reduces both handling and
subscriptions. `--strace-only`, for example, is a compatibility diagnostic and
not a determinism configuration.

Record/replay has a separate 49-name subscription. Its union with the default
run set contains 108 names: it additionally traps `access`, `fadvise64`,
`fchdir`, `getpeername`, `getsockname`, `getsockopt`, `ioctl`, `lseek`, `mkdir`,
`mprotect`, `pread64`, `pwrite64`, `pwritev`, `pwritev2`, `readlink`, `sendmsg`,
`sendto`, `setsockopt`, `settimeofday`, `unlink`, `unlinkat`, and `writev`.
Recording those calls does not expand normal run-mode determinization.

## Trapped Coverage Matrix

The following rows exhaust the 86 default optimized subscriptions.

| Area | Syscalls | Treatment | Determinism assessment |
| --- | --- | --- | --- |
| Virtual time and sleeps (8) | `gettimeofday`, `time`, `clock_gettime`, `clock_getres`, `nanosleep`, `clock_nanosleep`, `alarm`, `pause` | Time queries are emulated or overwritten from logical time. Sleeps and alarms use scheduler resources when serialization is enabled. | Strong for supported clocks/flags under default settings. Unsupported `clock_nanosleep` flags and scheduler-disabled paths inject host operations. `gettimeofday` still invokes Linux before overwriting the timeval. |
| Virtual identity/random/system data (4) | `getrandom`, `getcpu`, `uname`, `sysinfo` | PRNG bytes, CPU/node zero, normalized UTS fields, and mostly synthetic system data. | `getrandom` and `getcpu` are deterministic from configuration/seed. `uname` retains some kernel-provided fields. `sysinfo.free_ram` uses live resident-page data, so the structure is not fully virtual. |
| File metadata and directory order (7) | `stat`, `lstat`, `fstat`, `newfstatat`, `statx`, `getdents`, `getdents64` | Linux resolves the object; Detcore replaces inode/timestamps and sorts directory entries. | Partial. Object existence, permissions, type, size, contents, and I/O errors remain filesystem inputs. `fstat` has a source FIXME noting incomplete normalization. |
| File, memory, and descriptor state (18) | `open`, `openat`, `creat`, `close`, `read`, `write`, `mmap`, `munmap`, `mremap`, `fcntl`, `dup`, `dup2`, `dup3`, `pipe`, `pipe2`, `utime`, `utimes`, `utimensat` | Operations are injected with path/FD resource ordering and bookkeeping; random-device reads are emulated; deterministic I/O retries short reads/writes. | Partial. Stable regular files are conditionally repeatable. File contents/errors remain inputs, memory mapping is explicitly incomplete, and most `fcntl` commands are passthrough. Deterministic I/O controls transfer length, not external data. |
| Process/thread lifecycle (10) | `clone`, `clone3`, `fork`, `vfork`, `execve`, `execveat`, `exit`, `exit_group`, `wait4`, `setsid` | Detcore registers children, orders exit/wait, updates FD state across exec, and injects lifecycle calls. | Partial. PID/TID values are not virtualized. `vfork` is implemented as `fork`; `CLONE_VFORK` is unsupported. Executable/filesystem errors remain external. |
| Synchronization and CPU policy (4) | `futex`, `sched_yield`, `sched_getaffinity`, `sched_setaffinity` | Supported futex WAIT/WAKE and BITSET operations are modeled; yield is a scheduler turn; affinity is virtualized to CPU 0. | Strong only for supported futex operations under the precise default mode. Other futex commands panic; scheduler-disabled and alternate debug modes inject or poll Linux. |
| Signal API (5) | `rt_sigaction`, `rt_sigprocmask`, `rt_sigtimedwait`, `signalfd`, `signalfd4` | Protects Reverie's reserved signal, deterministically polls timed waits, and tracks signal FDs. | Partial. The kernel still owns masks/actions and signal-FD contents; signal sources and untrapped signal syscalls can remain external. |
| Special FD creation/management (11) | `eventfd`, `eventfd2`, `timerfd_create`, `timerfd_settime`, `timerfd_gettime`, `inotify_init`, `inotify_init1`, `inotify_add_watch`, `inotify_rm_watch`, `memfd_create`, `userfaultfd` | Linux executes the call; Detcore records FD type/flags and delegates timer/inotify operations. | Mostly bookkeeping. Event counts, timer expiration, inotify events, userfault activity, and subsequent unmodeled FD operations can expose host timing or mutable state. |
| Polling and networking (13) | `socket`, `socketpair`, `bind`, `connect`, `accept`, `accept4`, `recvfrom`, `poll`, `epoll_create`, `epoll_create1`, `epoll_ctl`, `epoll_pwait`, `epoll_wait` | Sockets are made physically nonblocking, local blocking operations are retried through scheduler turns, and port zero is assigned from a deterministic range. | Conditional for isolated, guest-internal communication. External endpoints are a record/replay boundary. Poll cannot currently distinguish internal from external FD sets, and `epoll_pwait` is injected as a blocking call. |
| Key management (3) | `add_key`, `request_key`, `keyctl` | Trapped, then explicitly passed through. | Not deterministic. Kernel key serial numbers and keyring state are not virtualized. |
| Intentional rejection (3) | `futimesat`, `epoll_ctl_old`, `epoll_wait_old` | `futimesat` returns `ENOSYS`; obsolete epoll ABIs panic. | Deterministic failure, not compatibility. |

### Dispatch/Subscription Drift

The dispatch match contains handlers for `recvmsg`, `sendto`, `sendmsg`, and
`sendmmsg`, but optimized run mode subscribes only to `recvfrom`. Those four
calls therefore bypass their nonblocking scheduler handler in normal release
builds. `recvmmsg` is explicitly TODO and has neither a run subscription nor a
handler.

The match also has named passthrough arms for common runtime calls (`brk`,
`access`, `mprotect`, `arch_prctl`, `set_tid_address`, `set_robust_list`,
`prlimit64`, `readlink`, `readlinkat`, `madvise`, `prctl`, and `sigaltstack`).
None is subscribed by default run mode. The arms are reachable in debug builds
or when another sub-tool adds a subscription. In an optimized normal run,
`--panic-on-unsupported-syscalls` cannot diagnose them.

## Direct Passthrough Matrix

Every name below bypasses Detcore in a default optimized run. The classification
is conservative and exhaustive: the four classes contain 287 unique names.

| Class | Count | Meaning |
| --- | ---: | --- |
| C: conditional | 111 | Usually repeats only when paths, files, descriptors, credentials, limits, address layout, and concurrent mutation are fixed. Hermit does not enforce those preconditions for the call. |
| N: nondeterministic/model-breaking | 137 | Commonly exposes time, IDs, scheduling, readiness, mutable kernel state, host topology, or external input; may block outside Detcore or create/mutate objects absent from Detcore's model. |
| P: privileged/legacy | 37 | Usually fails under the guest's namespace/capability policy or is obsolete, but errno/support still varies by host kernel and policy. |
| S: backend-special | 2 | Direct execution is required by Reverie's restart/signal protocol; determinism depends on the surrounding trapped event path. |

### C: Conditional On Frozen Inputs

These are not security guarantees. For example, `pread64` from an immutable
regular file at a fixed offset is repeatable; the same family used against a
device, procfs entry, pipe, or externally modified file is not. Vectored and
zero-copy calls are similarly descriptor-dependent.

```text
access arch_prctl brk capget capset chdir chmod chown copy_file_range faccessat faccessat2
fadvise64 fallocate fchdir fchmod fchmodat fchmodat2 fchown fchownat fdatasync fgetxattr
flistxattr fremovexattr fsetxattr fsync ftruncate get_robust_list getcwd getegid geteuid getgid
getgroups getresgid getresuid getrlimit getuid getxattr lchown lgetxattr link linkat listxattr
llistxattr lremovexattr lseek lsetxattr madvise map_shadow_stack mkdir mkdirat mknod mknodat
mlock mlock2 mlockall modify_ldt mprotect msync munlock munlockall personality pkey_alloc
pkey_free pkey_mprotect pread64 preadv preadv2 prlimit64 pwrite64 pwritev pwritev2 readahead
readlink readlinkat readv remap_file_pages removexattr rename renameat renameat2 rmdir sendfile
set_robust_list set_tid_address setdomainname setfsgid setfsuid setgid setgroups sethostname
setregid setresgid setresuid setreuid setrlimit setuid setxattr sigaltstack splice symlink
symlinkat sync sync_file_range syncfs tee truncate umask unlink unlinkat vmsplice writev
```

### N: Nondeterministic Or Model-Breaking

This class includes calls that can sleep without a Detcore scheduler resource,
observe asynchronous readiness, return non-virtual IDs or host statistics,
access external networking, or create FDs/state that Detcore never records.
`openat2`, `close_range`, `pidfd_*`, `io_uring_*`, `fanotify_*`, and
`memfd_secret` are especially dangerous because later trapped FD calls can see
a descriptor table different from Detcore's model.

```text
adjtimex bpf cachestat clock_adjtime clock_settime close_range epoll_pwait2 fanotify_init
fanotify_mark flock fsconfig fsmount fsopen fspick fstatfs futex_requeue futex_wait futex_waitv
futex_wake get_mempolicy getitimer getpeername getpgid getpgrp getpid getppid getpriority getrusage
getsid getsockname getsockopt gettid io_cancel io_destroy io_getevents io_pgetevents io_setup
io_submit io_uring_enter io_uring_register io_uring_setup ioctl ioprio_get ioprio_set kcmp kill
landlock_add_rule landlock_create_ruleset landlock_restrict_self listen listmount lsm_get_self_attr
lsm_list_modules lsm_set_self_attr mbind membarrier memfd_secret migrate_pages mincore mount
mount_setattr move_mount move_pages mq_getsetattr mq_notify mq_open mq_timedreceive mq_timedsend
mq_unlink msgctl msgget msgrcv msgsnd name_to_handle_at open_tree openat2 perf_event_open
pidfd_getfd pidfd_open pidfd_send_signal ppoll prctl process_madvise process_mrelease
process_vm_readv process_vm_writev pselect6 recvmmsg recvmsg rseq rt_sigpending rt_sigqueueinfo
rt_sigsuspend rt_tgsigqueueinfo sched_get_priority_max sched_get_priority_min sched_getattr
sched_getparam sched_getscheduler sched_rr_get_interval sched_setattr sched_setparam
sched_setscheduler seccomp select semctl semget semop semtimedop sendmmsg sendmsg sendto
set_mempolicy set_mempolicy_home_node setitimer setns setpgid setpriority setsockopt settimeofday
shmat shmctl shmdt shmget shutdown statfs statmount tgkill timer_create timer_delete
timer_getoverrun timer_gettime timer_settime times tkill unshare waitid
```

### P: Privileged Or Legacy

Deterministic `EPERM`/`ENOSYS` on one CI host is not a portable guarantee. A
different kernel, capability set, or user namespace may execute these calls and
expose or mutate host-dependent state.

```text
_sysctl acct afs_syscall chroot create_module delete_module finit_module get_kernel_syms
get_thread_area getpmsg init_module ioperm iopl kexec_file_load kexec_load lookup_dcookie
nfsservctl open_by_handle_at pivot_root ptrace putpmsg query_module quotactl quotactl_fd reboot
security set_thread_area swapoff swapon sysfs syslog tuxcall umount2 uselib ustat vhangup vserver
```

### S: Backend-Special

```text
restart_syscall rt_sigreturn
```

## Highest-Risk Gaps

| Priority | Gap | Consequence | Suggested first boundary |
| --- | --- | --- | --- |
| P0 | Optimized subscriptions do not cover handled `recvmsg`/send-family calls. | Socket calls can block or complete according to host timing without a scheduler event; debug tests exercise different behavior. | Subscribe the implemented calls and add an optimized-build subscription test. Implement `recvmmsg` timeout semantics. |
| P0 | Modern FD creators/mutators are untracked: `openat2`, `close_range`, `pidfd_open`, `pidfd_getfd`, `memfd_secret`, `fanotify_init`, `io_uring_setup`, and mount-FD APIs. | Detcore's FD type/flags/resource map diverges; later trapped `read`, `write`, `close`, `fcntl`, or polling may fail or use the wrong model. | Trap and update FD bookkeeping, or return a documented deterministic error until modeled. |
| P0 | `select`, `pselect6`, `ppoll`, `epoll_pwait2`, new futex calls, and `waitid` bypass blocking control. | A guest thread can block in Linux outside Detcore's timed-wait/run-queue model, causing hangs and timing-dependent wake order. | Add nonblocking retry/scheduler adapters matching `poll`, `epoll_wait`, futex, and `wait4`. |
| P1 | Positioned/vectored and zero-copy I/O bypass deterministic I/O and resource ordering. | Common runtimes, databases, and file servers expose short-I/O, file-offset, pipe, and socket timing differences. | Cover `pread*`, `pwrite*`, `readv`, `writev`, `sendfile`, `copy_file_range`, `splice`, `tee`, and `vmsplice`, classifying the FD type. |
| P1 | POSIX/interval timers and accounting clocks use host time. | `getitimer`, `setitimer`, POSIX timers, `times`, and `getrusage` reveal wall/CPU timing and signal races. | Back timers with logical time and scheduler events; normalize CPU accounting or reject it. |
| P1 | PID/TID and signal-target APIs are not virtualized. | IDs vary across namespace/host conditions and feed `kill`, pidfd, `/proc`, logs, and application protocols. | Define stable virtual IDs and translate syscall arguments/results consistently. |
| P1 | `ioctl` and `prctl` are command-dependent catch-all holes. | Individual commands expose devices, terminals, CPU features, clocks, namespace state, or alter execution policy. | Inventory commands from representative workloads; explicitly model/allowlist deterministic commands and reject or record the rest. |
| P2 | System V/POSIX IPC, AIO, and io_uring have no deterministic model. | Blocking, wake order, completion order, IDs, and shared kernel state are uncontrolled. | Start with deterministic rejection for unsupported async engines; add IPC models only for demonstrated workloads. |
| P2 | Filesystem namespace and metadata operations are mostly direct. | Stable images often repeat, but external mutation, allocation, xattrs, statfs, and filesystem-specific errors escape Hermit. | Document immutable-filesystem requirements, then add resource ordering and output normalization for high-frequency calls. |

## Prioritized Roadmap And Effort

Effort assumes one engineer familiar with Reverie/Detcore and includes focused
tests plus optimized-build coverage checks.

1. **Make coverage testable (2-3 days).** Expose or unit-test the optimized
   subscription set; assert that every intentionally handled syscall is either
   subscribed or annotated as debug/record-only. Add a generated count/list
   check so debug builds cannot mask release omissions.
2. **Close existing handler drift (2-4 days).** Subscribe `recvmsg`, `sendto`,
   `sendmsg`, and `sendmmsg`; validate local socket scheduling; implement or
   deterministically reject `recvmmsg`.
3. **Protect FD-model integrity (1-2 weeks).** Add `openat2` and `close_range`
   first, then pidfd/memfd/fanotify/mount-FD creators. A deterministic rejection
   is safer than silent model divergence when full support is not ready.
4. **Cover mainstream blocking multiplexers (1-2 weeks).** Implement
   `select`, `pselect6`, `ppoll`, `epoll_pwait2`, `waitid`, and new futex wait
   APIs using existing scheduler/timed-wait helpers.
5. **Cover mainstream I/O families (1-2 weeks).** Generalize deterministic
   read/write logic to positioned, vectored, and zero-copy operations, with
   regular-file versus pipe/socket policy.
6. **Virtualize remaining timer APIs (1-2 weeks).** Model interval/POSIX timers
   and normalize process accounting. Add signal interruption and cancellation
   tests.
7. **Virtualize process identity (3-6 weeks).** Introduce PID/TID translation
   across getters, signal targets, wait/pidfd, `/proc` interactions, and logs.
   This is broad and should be designed before incremental syscall patches.
8. **Define explicit policy for open-ended APIs (1 week initially).** Add
   allowlist/reject/record decisions for `ioctl`, `prctl`, `seccomp`, BPF,
   io_uring/AIO, IPC, and privileged administration. Full emulation is
   workload-driven and can take multiple additional weeks per subsystem.

The first four steps unblock the largest set of modern libc, language runtime,
network service, and database workloads while also preventing silent
determinism failures. Full Linux syscall parity is not a useful Phase 1 target;
an explicit deterministic rejection is preferable to an invisible direct
passthrough for unsupported stateful or blocking operations.

## Verification And Maintenance

The matrix was produced by comparing:

- `Detcore::subscriptions` and `Detcore::handle_syscall_event` in
  `detcore/src/lib.rs`;
- behavior in `detcore/src/syscalls/`;
- record/replay additions in `hermit-cli/src/recorder/mod.rs`;
- the pinned x86_64 enum in `syscalls` 0.6.18.

Re-run the audit whenever the syscall crate, subscription list, dispatch match,
or record/replay subscription changes. The mechanically checkable invariants
for this snapshot are `86 + 287 = 373` and `111 + 137 + 37 + 2 = 287`.
