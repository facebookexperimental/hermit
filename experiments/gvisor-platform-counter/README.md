# gVisor platform syscall counter

This experiment isolates gVisor's `platform.Context.Switch` boundary from the
rest of runsc. It maps a small amd64 guest loop directly into a gVisor memory
manager, executes it with either the systrap or KVM platform, counts each
syscall exit, supplies a synthetic result for `getpid`, and stops on
`exit_group`.

The gVisor overlay is pinned and validated against commit
`8eb8f9e0df89e0352305057c2c08a993fe92bc03`.

## Scope boundary

This is a raw platform microbenchmark, not a replacement for runsc and not a
general application runner. The platform interface provides address spaces,
execution contexts, and exits. It does not provide an ELF loader, file
descriptors, process state, or Linux syscall implementations.

At the pinned revision:

- `pkg/sentry/platform/platform.go` documents a nil `Switch` error as a guest
  syscall exit.
- `pkg/sentry/kernel/task_run.go` routes that exit to `Task.doSyscall`.
- `pkg/sentry/kernel/task_syscall.go` dispatches through the Sentry syscall
  table.

Consequently, executing `find`, `tar`, `gcc`, or another real Linux program
requires the Sentry OS. Adding those facilities here would recreate the runsc
layer that this experiment is intended to exclude. The counter therefore uses
a mapped synthetic `getpid` loop and compares it with an equivalent static
assembly loop under Reverie's counter.

Systrap syscall-site patching also requires `kernel.TaskFromContext` in
`usertrap.PatchSyscall`. A platform-only context has no Sentry task. The tool
explicitly disables patching and reports `"syscall_patching": false`; the
systrap row measures the SUD/seccomp signal path, not the patched fast path.

## Files

- `gvisor-overlay/counter/main.go`: platform API counter.
- `gvisor-overlay/counter/BUILD`: gVisor Bazel target.
- `fixtures/getpid_loop.S`: equivalent static Reverie guest.
- `install_and_build.sh`: installs the overlay into a gVisor checkout and
  builds it.
- `run_comparison.py`: validates counts and records matched wall-time samples.
- `RESULTS.md`: observed result and interpretation.
- `results/2026-07-25/`: machine-readable evidence from this host.

## Build

Clone gVisor and check out the validated revision:

```bash
with-proxy git clone https://github.com/google/gvisor.git /tmp/gvisor
git -C /tmp/gvisor checkout 8eb8f9e0df89e0352305057c2c08a993fe92bc03
BAZEL=/path/to/bazelisk \
  experiments/gvisor-platform-counter/install_and_build.sh /tmp/gvisor
```

The resulting binary is normally:

```text
/tmp/gvisor/bazel-bin/pkg/sentry/platform/counter/counter_/counter
```

`GVISOR_BAZEL_FLAGS` adds site-specific Bazel flags. gVisor's systrap build
generates both amd64 and arm64 handler blobs. The build host therefore needs
both cross libc sysroots even though this counter runs only on amd64.

This development host has cross-prefixed GCC binaries without either libc
sysroot. Validation used a disposable gVisor worktree with the arm64 half of
`tools/arch.bzl:arch_transition_impl` removed, plus a local override of the
canonical `+crosstool_extension+crosstool` repository whose amd64 tool prefix
was changed from `/usr/bin/x86_64-linux-gnu-` to `/usr/bin/`. Those are host
toolchain accommodations; neither change is part of the counter overlay.

## Run

The counter prints one JSON object per measured run:

```bash
/tmp/gvisor/bazel-bin/pkg/sentry/platform/counter/counter_/counter \
  --backend=systrap --syscalls=100000 --runs=5

/tmp/gvisor/bazel-bin/pkg/sentry/platform/counter/counter_/counter \
  --backend=kvm --syscalls=100000 --runs=5
```

`total_syscalls` is `getpid_syscalls + 1` because the terminating
`exit_group` is counted. `elapsed_ns` covers only the platform switch loop;
platform construction, mappings, and teardown are outside that value.

Run the matched Reverie comparison from the Hermit repository root:

```bash
experiments/gvisor-platform-counter/run_comparison.py \
  --gvisor-counter /tmp/gvisor/bazel-bin/pkg/sentry/platform/counter/counter_/counter \
  --reverie-counter /path/to/reverie/target/release/counter2 \
  --reverie-profile release \
  --cpu 8 \
  --output-dir /tmp/gvisor-platform-counter-results
```

The runner compiles the static fixture, checks exact counts, performs warmups,
rotates backend order, and writes `metadata.json`, `raw.tsv`, `summary.tsv`,
and per-run diagnostics. It refuses to overwrite an existing output directory.

Reverie observes one additional `execve` before the fixture loop, so its exact
expected count is `iterations + 2`; the gVisor platform counter expects
`iterations + 1`.

## Interpretation

The count comparison is the authoritative result: both platform backends
delivered every mapped guest syscall exit, and Reverie delivered every syscall
in the equivalent static process. Timing is only comparable when both binaries
use equivalent build profiles and the same idle pinned CPU.

The checked-in observation used optimized gVisor and a debug Reverie counter,
because the predecessor benchmark's available counter binary was debug-built.
Its timings are directional evidence, not a release-to-release performance
claim. See `RESULTS.md` for exact commands, medians, and limitations.
