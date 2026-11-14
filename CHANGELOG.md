# Hermit

Notable changes for users. Versions 0.5.0, 0.5.2, 0.5.5, 0.5.6 and 0.5.7
describe crate releases; 0.5.1 and 0.5.3 describe selected development
snapshots, and 0.5.4 is a package-version alignment. The 0.5.2 release
branches from 0.5.0.

## Unreleased

- Determinism loss: a run that claims determinism (Hermit's default thread
  sequentialization, with or without `--strict`) and records a determinism
  loss is now refused with exit 122, after the guest's output, which is
  kept. Hermit prints `determinism loss recorded: <reason>` for each reason
  and `HERMIT_POLICY_REFUSAL class=policy-refusal cause=determinism-loss`.
  Before, a plain run exited with the guest's status and said nothing, for
  example after a cross-process `SIGKILL` on the in-guest backends
  (https://github.com/rrnewton/hermit/issues/5459).
  - `--verify` exits 122 with the same class line, where it exited 125 with
    `class=cli-error`. So does `log-diff --verify-json` for a saved
    `--verify-run` directory that recorded a loss, where it exited 125.
  - A `--verify-run` that recorded a loss and ended with a deadlock or a
    virtual timeout keeps that outcome's status, and its record saves the
    loss, so no `--verify-allow` lets `log-diff --verify-json` compare it.
  - A run with `--no-sequentialize-threads` keeps the guest's status and
    prints the losses it records, a partial list (the user guide names the
    known gaps); a run that already failed (`--timeout`, a
    scheduler deadlock) keeps its own status and class line and prints them.
  - `--summary-json` reports `determinism_loss` (`null`, or the reasons and
    their distinct total).

## 0.5.7

Release for the 2026-11-11 fbsource import. It pins Reverie 0.5.7
(`8143cbd8`) and summarizes the changes since 0.5.6.

- KVM `smaps`: Detcore renders `/proc/<pid>/smaps` for every mapping in the
  backend's memory map. Before, it rendered the initial stack and refused
  the read at the first other mapping.
  - `VmFlags` come from the flags the backend gives each mapping, named in
    Linux's bit order.
  - `THPeligible` follows Linux's rules. For a file on overlayfs or tmpfs
    it is the host kernel's answer for a mapping of the same file, which
    Reverie 0.5.7 asks for. In an address space that called
    `PR_SET_THP_DISABLE` it is 0.
  - What no fact decides is still refused by name, for example a mapping
    that `madvise` advice or `mlock` changed.
- KVM speed: with Reverie 0.5.7, a guest's file `mmap` no longer makes the
  backend read its whole `/proc/self/mountinfo`. Two KVM runs of a git
  workload in a container with 886 mounts had spent 1.81 s of their 6.28 s
  on those reads.
- KVM signals: Detcore answers a signal sent to a target that is not live
  (a zombie, a reaped process or an exited thread) from its own model of
  the run, at the sender's turn, instead of forwarding it to the backend.
  `tgkill`, `tkill` and `rt_tgsigqueueinfo` to a zombie leader answer 0, as
  on Linux, where they answered `ESRCH`, and an invalid signal number sent
  to a reaped process answers `ESRCH`, where it answered `EINVAL`.
  `rt_sigqueueinfo` and `pidfd_send_signal` to a zombie now answer from the
  same model, after Linux's siginfo checks. ptrace still leaves these sends
  to the kernel.
- In-guest SIGALRM: on `--backend in-guest-trap` with guest SIGALRM handlers
  admitted (the per-run opt-in `REVERIE_LITEINST_SIGALRM_HANDLERS=1`, with
  site patching off), a handler without `SA_RESTART` now interrupts a
  blocking read of a pipe that only guest processes hold, as on Linux. The
  read returns `EINTR` after the handler runs; data or EOF that arrived
  first is returned first; a child's exit interrupts nothing. Before, every
  such read was refused with `EOPNOTSUPP`.
  - These reads are still refused with `EOPNOTSUPP`, and a warning names
    the reason: under an `SA_RESTART` handler; of a named FIFO or the
    container's stdio; under any recording or replay; after a descriptor
    may have left the guest; and wherever Hermit cannot prove the pipe's
    status flags current, such as after another process changed them
    through a forked copy, or after the pipe end was sent or received over
    `SCM_RIGHTS`.
  - If what admitted a read stops holding before the read ends, the run
    stops with exit 122 and `HERMIT_SIGALRM_PIPE_STATUS_DIVERGED` or
    `HERMIT_SIGALRM_PIPE_AUTHORITY_REVOKED`, and the guest sees no result.
- `--verify` and `--strict`: the CPU identity in verification reports and
  in the refusal of an inexact branch counter now comes from reading only
  the first processor's block of `/proc/cpuinfo`: one 4 KiB read, where the
  whole file on a 316-CPU host is 617,230 bytes and about 2 ms of CPU.
- Pinned CPUs: at `--log=info` a pinned run logs one `pinned CPU placement`
  line with its CPU, how it holds it (reserved, kept, shared or
  unreserved), and whether its reservation coordinates through a host lock
  directory or only within its network namespace.
- Buck: the 30 BUCK files and 3 `.bzl` files that Hermit shares with
  fbsource were brought in line with fbsource's copies; the open-source shim
  supplies stand-ins for the internal `.bzl` files they load. Release targets are defined in
  `hermit-cli/release_alias.bzl`, and targets that need internal-only inputs
  are marked broken-in-oss. The registry release projection now sets
  `build-identity` in hermit-cli's Cargo manifest only.
- Packaging: the `oss_cargo/` manifest copies are retired. Each crate's
  `Cargo.toml` exists once, at its crate path.
- Align every first-party package and internal requirement at 0.5.7, and
  pin Reverie 0.5.7.

Incompatible changes:

- In-guest runtime: Hermit and its in-guest runtime
  (`libdetcore_liteinst.so`) check one protocol revision, which moves from
  56 to 57, so a 0.5.6 runtime and a 0.5.7 Hermit refuse each other by
  name.

## 0.5.6

Release for the 2026-11-10 fbsource import. It pins Reverie 0.5.6
(`f2129d99`) and summarizes the changes since 0.5.5.

- No NUMA: Detcore presents a kernel built without NUMA on every backend.
  The deterministic virtual machine has one memory node.
  - The six memory-policy system calls (`set_mempolicy`, `get_mempolicy`,
    `mbind`, `migrate_pages`, `move_pages` and `set_mempolicy_home_node`)
    fail with `ENOSYS`.
  - These do not exist: `/proc/<pid>/numa_maps`, `/sys/devices/system/node`,
    `/sys/bus/node`, memory tiering's sysfs directories, and the NUMA and
    memory-policy tunables under `/sys/kernel/mm`.
  - A device on the pci, platform, cxl, dax, nd or vmbus bus has no
    `numa_node`, whatever path reaches it.
  - `/proc/vmstat` and `/proc/zoneinfo` leave out the NUMA counters.
  - `/proc/<pid>/sched` leaves out NUMA balancing's lines.
  - `Mems_allowed` prints as `1`.
- sysfs paths: Detcore walks each looked-up path as Linux does, and decides
  every pathname call by the object the walk reaches. A symbolic link or a
  `..` no longer reaches an object the machine does not have.
- sysfs seeks: a seek on a rendered sysfs attribute measures the end from the
  attribute's size, as Linux does. A seek past the largest file offset fails
  with `EINVAL`.
- LiteInst: on in-guest LiteInst, a `pause()` or `nanosleep()` wakes only for
  a signal the guest catches and has not blocked, as on Linux. Before, an
  ignored `SIGCHLD`, or a timer signal that the guest ignored or blocked,
  ended the sleep with `EINTR`, and no handler ran.
- Signals: suppose a `SIGALRM` comes due while a single-threaded process with
  a handler waits in a pipe read. Detcore now delivers it deterministically:
  - if no data has arrived, the read returns `EINTR`;
  - otherwise the read completes, and then the handler runs.
  Record and replay cover it. Before, the run recorded a determinism loss.
  This covers `SIGALRM` and pipe reads only.
- Signals: Detcore picks the thread that takes a kernel-sent process signal,
  such as a child's exit `SIGCHLD` or a timer's signal, by the mask each
  thread has in force, as Linux's `complete_signal` does. The temporary mask
  of a `ppoll`, `pselect6` or `sigsuspend` call counts as the thread's mask.
  - KVM: Detcore now makes this choice, as it already did on ptrace. Before,
    the backend chose the thread, or none. Detcore also reads each sleeper's
    signal mask and dispositions on KVM, so by default a queued plain sleep
    registers at the first scheduler pass, as on ptrace
    (`--sleep-registration`).
  - ptrace: a signal that Linux discards when it is sent, because the guest
    ignores it and has not blocked it, no longer reaches a handler the guest
    installs later. Under ptrace the kernel queues such a signal anyway.
  - `--debug-futex-mode=polling`: a futex wait that a caught `SIGCHLD`
    reaches ends with `EINTR` when the child exits, as in the default precise
    mode and on Linux. Before, Hermit held the signal until the wait returned.
  - Known gaps on KVM: a POSIX timer whose signal is not `SIGALRM` still
    reaches no thread (https://github.com/rrnewton/hermit/issues/4995), and
    one case with a busy worker thread differs from Linux
    (https://github.com/rrnewton/reverie/issues/1300).
- Chaos: `--chaos-timed-wait-spread=FACTOR` wakes each sleep, and each futex
  wait with a timeout, after a seeded fraction of the other threads'
  progress, drawn log-uniform between 1/FACTOR and FACTOR. A race decided by
  a sleep against concurrent work can then change order between seeds. The
  waiter never observes less than the time it asked for, and a fixed seed
  reproduces the run; recorded schedules store the value and the seed. It
  needs `--chaos` and the ptrace backend. Poll, epoll and select timeouts and
  signal timers are not spread.
- Pinned CPUs: a run pinned to one CPU now says so before the guest starts
  when that CPU is shared with another Hermit run, cannot be reserved, or is
  reserved only within its network namespace, with a `hermit: warning:
  cpu-shared:`, `cpu-unreserved:` or `cpu-netns-only:` notice.
  `hermit run --pinned-cpu=refuse` or `--pinned-cpu=wait[:SECONDS]` refuses
  or waits instead of sharing (`share` is the default). A reservation also
  locks a file under `/dev/shm/hermit-pinned-cpu`, so runs in different
  network namespaces that share that directory see each other's
  reservations. `hermit run`, `hermit replay` and `hermit record start` take
  `--pinned-cpu-dir` and `--pinned-cpu-notice=on|off`.
- LiteInst: the in-guest runtime no longer allocates on the guest's heap
  before `main`. Its dispatch-stack list had registered a glibc thread-exit
  destructor at the first dispatch.
- Build: Detcore and the hermit-run library compile for aarch64 Linux. This is
  compile support only; x86_64 behaviour is unchanged.
- Align every first-party package and internal requirement at 0.5.6, and pin
  Reverie 0.5.6.

Incompatible changes:

- Recordings: the record format moves from `0x19e` to `0x1a0` and must match
  exactly, so 0.5.5 recordings are refused.
- A guest's memory-policy system calls fail with `ENOSYS`. 0.5.5 emulated one
  node for `get_mempolicy` and `move_pages`.

## 0.5.5

Release for the 2026-11-09 fbsource import. It pins Reverie 0.5.5
(`e39f78dd`) and summarizes the changes since the 0.5.4 alignment.

- Packaging: the core keeps the ptrace, KVM, LiteInst and in-guest-trap
  backends. DynamoRIO (DBT) moved to hermit-dynamorio, and SaBRe and e9patch
  to hermit-3rdparty-patching. `--backend` refuses their names instead of
  substituting another backend, and an outside crate can register a backend
  through `hermit::app::backend_hook`.
- Record/replay: replay is strict by default. A schedule replay stops at its
  first desync with `ERROR HERMIT_REPLAY_DESYNC` and exit 125. A difference
  only in polling phase, with matching guest state, resyncs. Schedules and
  preemption records name their branch-counter identity, and they are refused
  before the guest starts where the run cannot execute them. A replay names
  the host files that changed since the recording.
- ptrace and Detcore: programs that contend priority-inheritance mutexes or
  signal a multithreaded process now run instead of being refused.
  Descriptors received over `SCM_RIGHTS` are tracked, recorded and replayed
  instead of failing with `EBADF`. The CPU-time clocks and `clock()` report
  Detcore's CPU accounting instead of virtual wall time.
- Isolation and /proc: every run gets a private `/dev/shm`, its own IPC
  namespace and a private `/dev/mqueue` (`--bind /dev/shm` keeps the host's).
  `/proc/<pid>/stat` and `schedstat` show Detcore's CPU accounting instead of
  zeros, and Detcore renders `statm`, `io`, `arch_status` and
  `/proc/diskstats` itself on every backend.
- KVM: the preemption timer is on, and a late timer event refuses the run, as
  on ptrace. Detcore serves other guest processes' `/proc/<pid>` directories,
  `stat`, `comm`, `cmdline` and `environ`, so `ps`, `pgrep` and `pkill` see
  them. Detcore now decides `flock(2)` itself on every backend: KVM no longer
  refuses it, and a blocking `flock` waits instead of being refused.
- CPU profile: `--cpu-profile` defaults to `auto`. That presents x86-64-v2
  unless the executable or its program interpreter declares x86-64-v3, and
  then v3 on a host that qualifies. A guest that declares more than its
  profile presents is refused (exit 122). Replays and manifest reruns use the
  recorded profile.
- Chaos: `hermit record start --chaos --seed=N` records chaos schedules.
  `--chaos-timeslice-range=MIN..MAX` draws each slice's mean log-uniformly
  from a range, `--chaos-instruction-offset=N` lets a preemption land inside
  straight-line code, and `--virtual-cpus=N` (1 to 128) presents N CPUs so
  that thread pools start N workers.
- `hermit analyze`: `--prefix-search` localizes failures that depend on
  events only the failing run made, and the report names the racing system
  calls whose order flips. `--time-budget` and `--trial-budget` stop with a
  partial report that `--resume` continues. The workspace is removed on exit
  unless `--keep-tmp` is given.
- OCI images: `hermit run --image` reads images from podman's local store
  instead of pulling them through buildah, and takes the default program from
  the image's Entrypoint and Cmd. It runs on KVM as well as ptrace, and it
  accepts digest-verified `oci-dir:` layouts and `docker-archive:` tarballs.
  `hermit oci prune` removes unreferenced rootfs caches.
- aarch64 and Android: Hermit builds and runs on aarch64 Linux with the
  ptrace backend. `--rewrite-exclusives` rewrites load/store-exclusive retry
  loops into single LSE atomics, whose retries would otherwise shift
  preemption points. As root, a `--no-namespace` run chooses its guest
  process IDs from a base (`--pid-base`), and an Android guest reads a frozen
  copy of the system property area.
- Removed `scripts/update_spreadsheet.rs`: its inputs no longer exist (no
  `hermit_syscalls.csv` in the tree, and the v1 `../detcore` layout it assumed
  is gone), and nothing referenced it. Restore with `git show
  37ac767f2fe0529eb936bf6771c760fbd3a1185c^:scripts/update_spreadsheet.rs`.
- Align every first-party package and internal requirement at 0.5.5, and pin
  Reverie 0.5.5.

Incompatible changes:

- Recordings: the record format moves from `0x186` to `0x19e` and must match
  exactly, so 0.5.4 recordings are refused. So are schedule and preemption
  files that carry no branch-counter identity.
- Replay: the first desync, including a soft one, ends the replay with exit
  125. `--die-on-desync` is now a hidden no-op that prints a warning.
- `--backend=dbt`, `sabre` and `e9patch` are refused, and the `dbt`, `sabre`,
  `e9patch` and `third-party-backends` Cargo features are gone.
- A guest that talks to host processes through `/dev/shm` now needs
  `--bind /dev/shm`. `--namespace-only` guests no longer reach host System V
  and POSIX message-queue objects.
- The CPU profile default, for the CLI and for `Config::default()`, is `auto`
  instead of x86-64-v2.
- `hermit run --image` never pulls. Fetch first with `hermit oci pull` or
  `podman pull`.

## 0.5.4

- Package-version alignment for the native import
  ([5f53234f](https://github.com/rrnewton/hermit/commit/5f53234fbb0129114a34de8cb06dfdd7cde5d194)).
  Changes made between the 0.5.3 snapshot and this alignment have no entries
  of their own.

## 0.5.3 (Development snapshot)

- Read the calling process's `/proc/.../cmdline` from its current argument
  memory on ptrace and KVM, including rewritten arguments, thread aliases and
  positioned reads. Reading another process's command line remains a KVM gap.
- Freeze `/etc/ld.so.cache` inside each container when the host has one, so a
  package installation cannot replace the dynamic linker's cache mid-run.
- Reduce temporary allocations in syscall forwarding when no record/replay
  tool is active, while retaining the record/replay tool path for recordings.
- Allow the Redis integration workloads to use explicitly declared
  `redis-server` and `redis-cli` executables through `HERMIT_PREPARED_REDIS`,
  rejecting invalid declarations before starting either executable.
- Preserve interruption and exit requests through cleanup in the experimental
  private-runtime build and packaging tools.
- Align Hermit's package requirements and runtime source identities with the
  matching Reverie 0.5.3 snapshot.

[Selected source](https://github.com/rrnewton/hermit/commit/8d924a294e49a941b3a9c19e08e6bc86363d04ce).

## 0.5.2

- Fix `cargo install hermit-run` when dependency resolution selects libc
  0.2.190 or later.
- Restore `hermit-detcore` builds for wait and queued-signal handlers with
  libc's new `siginfo_t` layout.
- Restore `hermit-replay` builds for SIGPIPE delivery with that layout.
- Republish only `hermit-detcore` and `hermit-replay` at 0.5.2; `hermit-run`
  and the other crates remain at 0.5.0 and accept the compatibility update.
- Keep the locked 0.5.0 installation path usable with libc 0.2.189 and
  preserve the signal bytes presented to the guest.

[Release details](https://github.com/rrnewton/hermit/blob/v0.5.2/docs/releases/0.5.2.md).

## 0.5.1 (Development snapshot)

- Add `--cpu-profile=host-no-rdrand` for intercepted host CPU features with
  hardware randomness hidden. This requires ptrace, CPUID faulting and virtual
  time; results depend on the host CPU model and it is refused for record/replay.
- Add `--signal-at=turn=N:SIGNAL` to send a catchable signal to the run's root
  process after a committed scheduler turn. Supported on ptrace, including
  e9patch preprocessing; unsupported execution modes refuse the option.
- Add `--work-span-report=<path>` to report schedule work and critical-path
  estimates, with the unmodelled dependencies listed in the report.
- Add `--strict-schedule-boundaries` for recording and checking software slice
  boundaries during strict trace replay. Refuse legacy schedules in this mode,
  mismatches, incomplete replays and unrecorded branch fragments.
- Recognize random devices by device identity, including aliases of
  `/dev/random` and `/dev/urandom`, so deterministic reads also cover those
  spellings during record/replay.
- Include stat-family output hashes and decoded metadata fields in INFO I/O
  logs, allowing a metadata divergence to be located at the producing syscall.
- Resolve relative guest program paths before `hermit-verify` changes working
  directory, while preserving bare program names as guest `PATH` lookups.
- Name the CLI Cargo package `hermit-run`, retaining `hermit` as the installed
  executable and Rust library name.

[Selected source](https://github.com/rrnewton/hermit/commit/02a85cc45279b9dbbf83f4decf680bc425918f34).

## 0.5.0

- Ship ptrace as the default and include KVM in the registry package. Optional
  SaBRe support and e9patch preprocessing need external runtimes; e9patch
  executes through ptrace. DBT, LiteInst, in-guest-trap and native-exec remain
  outside the registry release.
- Support `FUTEX_REQUEUE`, `FUTEX_CMP_REQUEUE` and `FUTEX_WAKE_OP` in precise
  futex mode with sequentialized execution. Priority inheritance remains
  unsupported.
- Preserve temporary signal masks across interrupted blocking calls during
  record/replay, and restart interrupted initial mask probes instead of
  exposing a spurious `EINTR`.
- Select filtered procfs views with `--proc=minimal|standard`, defaulting to
  `minimal`. Omitted names return `ENOENT`; exposed files retain their
  individual modelling limits.
- Add experimental `--record-networking=<path>` and
  `--replay-networking=<path>` for outbound TCP recording and offline replay,
  checking sent bytes. UDP sends and listening-server operations are refused;
  remaining host namespace access can still make a recording diverge on replay.
- Extend experimental ptrace syscall recording/replay to `accept` and
  `accept4`, including peer-address output and copyout errors. Recording format
  `0x17e` is incompatible with 0.4.1 recordings.
- Save and load options with `--save-config` and `--config`. Run manifests
  retain resolved epoch/seed inputs; recording commands accept IDs, unique
  prefixes, paths, `last` and `last-run`.
- Capture verification executions separately with `--verify-run` and compare
  them with `hermit log-diff --verify-json`. Verification refuses known
  determinism losses and log-write failures; canonical observation comparison
  requires `--verify-strict`.
- Require an explicit epoch and reproducible seed inputs under `--strict`.
  Strict ptrace runs refuse invalid branch counters when their counts would
  affect execution.
- Add syscall/descriptor occurrence and completion anchors to happens-before
  constraints. Add `--scheduler-turn-cost`, `--futex-wake-yields` and
  `--pipe-wake-latency` for the supported scheduling modes.
- Debug live ptrace runs through `--dap-listen`, the `hermit-dap` adapter and
  GDB, with early refusals for unsupported launch combinations.
- Offer `--cpu-profile=x86-64-v3` on qualified ptrace hosts, retaining
  x86-64-v2 as the default. Record/replay binds and checks host XSAVE facts;
  the qualification does not cover every VM-exit or firmware effect.

[Release source](https://github.com/rrnewton/hermit/tree/v0.5.0).

## 0.4.1

- Fix stable Rust installation of `hermit-run`, including Ubuntu
  libunwind/LZMA linking and default-feature dependency selection.
- Refuse disabled SaBRe and e9patch paths before artifact resolution or
  recording setup, including before captured-stdin preparation. Explain when
  DBT requires a source build.
- Report explicitly when a source revision was not embedded in version output,
  while retaining the published package version.
- Adopt Reverie's adaptive waits, qualified for ptrace on Ubuntu 24.04's Linux
  6.8 kernel. Generic sibling-thread waits need Linux 6.9 thread pidfds;
  Linux 5.15 remains an unqualified design target.
- Document stable installation, scoped AppArmor setup and reuse of reported
  epoch/seed inputs. Precise preemption needs usable performance counters;
  nested KVM qualification remains deferred.

[Release source](https://github.com/rrnewton/hermit/tree/v0.4.1).

## 0.4.0

The earliest tagged baseline covered here targets x86_64 Linux.

- Publish the CLI as the `hermit-run` Cargo package, preserving the `hermit`
  executable and Rust library names, with matching versioned support crates.
- Include ptrace and KVM by default. SaBRe, LiteInst and e9patch preprocessing
  are opt-in and need runtime artifacts; DBT is outside the registry package.
  e9patch preprocessing uses ptrace.
- Provide configurable scheduling, virtual time and randomness. An omitted
  epoch is captured from the host; preserve the reported epoch and seed for
  repeat time-dependent output. Strict mode refuses incompatible opt-outs.
- Compare two runs with `--verify`, including syscall output-buffer hashes by
  default. Use `--verify-strict` for canonical observation comparison; the
  default comparison is diagnostic.
- Provide experimental syscall recording and GDB or autopilot replay, sharing
  run/container configuration and retaining the recording's network mode.
  A replay match does not establish determinism between independent recordings.

[Release source](https://github.com/rrnewton/hermit/tree/v0.4.0).
