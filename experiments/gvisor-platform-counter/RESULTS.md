# Observed result: 2026-07-25

## Environment

- Host: Linux `6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79`, x86-64.
- CPU: AMD EPYC 9D85 158-Core Processor.
- Affinity: logical CPU 111 for every measured command.
- gVisor source: `8eb8f9e0df89e0352305057c2c08a993fe92bc03`.
- Reverie source: `62e7593c96aa2e7b42189e80de326528b52133c7`.
- Workload: 100,000 raw `getpid` instructions followed by `exit_group`.
- Samples: five separate processes per backend, no warmups in this focused
  observation. Backend order rotated on each sample.
- gVisor profile: Bazel `-c opt`.
- Reverie profile: Cargo `debug` (`counter2`).

## Exact-count result

Every sample exited zero.

| Counter | Expected | Observed in all 5 samples |
| --- | ---: | ---: |
| gVisor platform / systrap | 100,001 | 100,001 |
| gVisor platform / KVM | 100,001 | 100,001 |
| Reverie ptrace `counter2` | 100,002 | 100,002 |

The gVisor total contains 100,000 `getpid` exits plus `exit_group`. Reverie
also observes the process's initial `execve`.

## Timing observation

| Counter | Median process wall time | Wall ns / counted syscall | Median platform switch loop | Switch-loop ns / syscall |
| --- | ---: | ---: | ---: | ---: |
| gVisor systrap, unpatched | 838,128,817 ns | 8,381.20 | 821,488,300 ns | 8,214.80 |
| gVisor KVM | 316,140,049 ns | 3,161.37 | 93,823,041 ns | 938.22 |
| Reverie ptrace `counter2` debug | 3,311,500,428 ns | 33,114.34 | n/a | n/a |

Process wall time includes counter initialization, guest setup or `execve`,
the loop, and teardown. The gVisor switch-loop metric begins after platform,
memory manager, mappings, and context setup. Reverie does not expose an
equivalent internal interval, so only process wall time is cross-tool scope.

The observed debug Reverie wall cost was 3.95 times the unpatched systrap wall
cost and 10.47 times the KVM wall cost. These are not release-to-release ratios:
the gVisor binary was optimized and the available Reverie counter was a debug
build. Raw samples are in `results/2026-07-25/raw.tsv`.

## Commands and observed output

The process commands used for the raw rows were:

```text
taskset -c 111 counter --backend=systrap --syscalls=100000 --runs=1
taskset -c 111 counter --backend=kvm --syscalls=100000 --runs=1
gcc -nostdlib -static -x assembler -o /tmp/reverie-getpid-loop-100k -
taskset -c 111 counter2 /tmp/reverie-getpid-loop-100k
```

Each command was invoked five times. Wall time was measured around the complete
process with `date +%s%N`; the gVisor JSON supplied the switch-loop interval.

Build:

```text
with-proxy bazelisk build \
  --override_repository=+crosstool_extension+crosstool=/tmp/gvisor-counter-crosstool \
  --config=x86_64 -c opt //pkg/sentry/platform/counter:counter
```

Observed: exit 0, `Build completed successfully`, binary
`bazel-bin/pkg/sentry/platform/counter/counter_/counter`.

Backend smoke tests after the final source change:

```text
counter --backend=systrap --syscalls=1000 --runs=1
```

Observed: exit 0, `getpid_syscalls=1000`, `total_syscalls=1001`,
`syscall_patching=false`.

```text
counter --backend=kvm --syscalls=1000 --runs=1
```

Observed: exit 0, the same exact counts and `syscall_patching=false`.

Refusal checks:

```text
counter --backend=ptrace --syscalls=1
counter --backend=systrap --syscalls=0
counter --backend=kvm --device=/definitely/missing --syscalls=1
```

Each exited 1. Diagnostics respectively named the unsupported backend, rejected
zero iterations, and reported the missing KVM device.

## Limits

This tool cannot load or run the predecessor benchmark's real applications at
the platform layer. Those applications require the Sentry ELF loader and Linux
syscall implementations, which restores full runsc overhead. The platform-only
comparison is therefore restricted to the raw syscall loop.

The systrap result is deliberately unpatched. Syscall-site patching requires a
Sentry `kernel.Task`, which the platform-only tool does not create. The result
measures systrap's fail-closed SUD/seccomp signal path.

No Hermit assurance level applies to this experiment: these commands invoke
gVisor's platform package and Reverie's standalone ptrace counter directly,
with default logging and no Hermit determinism relaxations or guarantees.
