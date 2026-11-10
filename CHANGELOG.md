# Hermit

Notable changes for users. Versions 0.5.0, 0.5.2 and 0.5.5 describe crate
releases; 0.5.1 and 0.5.3 describe selected development snapshots, and 0.5.4
is a package-version alignment. The 0.5.2 release branches from 0.5.0.

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
