# Hermit

[Hermit](https://hermetic-infra.org) runs Linux programs with controlled thread
schedules, time and randomness for reproducible testing and record/replay.
The Cargo package is **`hermit-run`**; its main executable is **`hermit`**.

Read the [2022 introduction, Hermit: Deterministic Linux for Controlled Testing
and Software Bug-finding](https://developers.facebook.com/blog/post/2022/11/22/hermit-deterministic-linux-testing/)
for the ideas behind the project, and the
[current user guide](https://github.com/rrnewton/hermit/blob/main/docs/USER_GUIDE.md)
for supported commands and execution guarantees.

## Install on Ubuntu 24.04

Use x86_64 Linux and a current **stable Rust** toolchain from
[rustup](https://rustup.rs/). The stock Ubuntu 24.04 **Linux 6.8** kernel is the
oldest kernel qualified for the 0.4.1 ptrace release. Linux user, PID and mount
namespaces, parent-child ptrace and seccomp filters must be available.

Hermit uses Reverie's explicit ptracer-thread mode on older kernels. The generic
Safeptrace interface for sibling-thread waits requires `PIDFD_THREAD`, introduced
in Linux 6.9, and reports an error when it is unavailable. Linux 5.15 is a design
target, with no qualification claim yet; see the
[compatibility follow-up](https://github.com/rrnewton/reverie/issues/943).

Install the C compiler/linker and native development libraries before Cargo:

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libunwind-dev liblzma-dev
cargo +stable install hermit-run --version 0.4.1 --locked
hermit --version
hermit run -- /bin/echo hello
```

No extra LZMA linker flags or nightly Rust are required. On other distributions,
install the equivalent C toolchain, pkg-config, libunwind and LZMA development
packages. The installer also provides `hermit-dap` and `verification-report`.

The default installation includes **ptrace and KVM**. SaBRe support, e9patch
preprocessing and LiteInst are off by default and require their explicit Cargo
features plus separately supplied runtime artifacts. DBT is available through
[source builds](https://github.com/rrnewton/hermit), rather than this registry
release. KVM requires read/write access to `/dev/kvm`, compatible CPU features
and performance counters; ptrace is the default execution path.

### Ubuntu AppArmor and user namespaces

Ubuntu 24.04 restricts unprivileged user namespaces through AppArmor. A first
run can fail with `MapUid: EPERM` or `Setting UID map failed` even when
`kernel.unprivileged_userns_clone=1`. Ask the system administrator to allow user
namespaces for the installed Hermit executable with a scoped AppArmor profile.
Keep the system-wide user-namespace restriction enabled.

Find the executable's absolute path with `readlink -f "$(command -v hermit)"`.
Have the administrator create `/etc/apparmor.d/hermit-local` with this content,
replacing `/home/YOUR_USER/.cargo/bin/hermit` with that exact path:

```text
abi <abi/4.0>,
include <tunables/global>

profile hermit /home/YOUR_USER/.cargo/bin/hermit flags=(unconfined) {
  userns,
}
```

Load it and retry:

```sh
sudo apparmor_parser -r /etc/apparmor.d/hermit-local
hermit run -- /bin/echo hello
```

This profile grants the installed Hermit executable user-namespace access while
keeping `kernel.apparmor_restrict_unprivileged_userns=1`. Review the executable
path and local security policy before applying it. Reinstalling to a different
path requires updating the profile.

### Performance counters

Precise preemption uses CPU performance counters. Host security policy and
virtual machines can restrict them. Hermit reports when it disables its PMU
timeslice; a successful small example in that mode does not qualify
CPU-bound or threaded workloads for precise preemption. Check the user guide
before changing the host's perf policy.

## Run and verify

```sh
hermit run -- /bin/echo hello
hermit run --strict -- /bin/echo hello
hermit run --strict --verify --verify-strict -- /bin/echo reproducible < /dev/null
```

`--strict` selects strict launch behavior. `--verify-strict` selects the full
canonical observation comparison; ordinary `--verify` uses a weaker diagnostic
comparison. Hermit does not make a changing filesystem or external network
reproducible; provide stable inputs for repeatability checks.

Virtual time starts at the current host time captured once for the invocation.
Hermit reports the exact epoch and `reproduce with --epoch=...` on stderr, or in
the selected `--log-file`. Reuse that epoch and the same seed for time-dependent
reproduction. Explicit `--epoch` and `HERMIT_EPOCH` values are preserved;
`--seed` defaults to 0. See the
[virtual-time behavior notes](https://github.com/rrnewton/reverie/issues/946).

## Record and replay

```sh
hermit record start -- /bin/echo recorded
hermit record list
hermit replay --autopilot
```

Autopilot replays the latest recording without GDB. For an interactive replay,
install GDB and use `hermit replay <record-id>`. Run `hermit --help` and the
subcommand's `--help` for options, output locations and verification reports.

## License

BSD-3-Clause. See `LICENSE`.
