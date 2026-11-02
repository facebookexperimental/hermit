# Hermit

Notable changes for users. Versions 0.5.0 and 0.5.2 describe crate releases;
0.5.1 and 0.5.3 describe selected development snapshots. The 0.5.2 release
branches from 0.5.0.

## 0.5.3 (Unreleased)

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
