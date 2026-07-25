# Shared futex verification

Date: 2026-07-22

Implementation under test: `b44d9396418c316abdbc38e9e41e5809da936c49`
from draft [PR #53](https://github.com/rrnewton/hermit/pull/53), stacked on
PR #37.

## Conclusion

The shared-futex rejection that blocked QEMU, Node.js, Java, and
process-shared pthread synchronization is fixed.

- Node.js `Worker` plus `SharedArrayBuffer`/`Atomics.wait` passed 3/3 runs.
- OpenJDK's eight-thread latch and atomic-counter workload passed 3/3 runs.
- The pthread workload passed 5/5 runs. It covers both ordinary thread
  synchronization and `PTHREAD_PROCESS_SHARED` mutex/condition variables in a
  `MAP_SHARED | MAP_ANONYMOUS` region across `fork`.
- Detcore's low-level cross-process shared anonymous futex regression passed
  its two-run determinism oracle.
- QEMU no longer aborts at its first process-shared futex. The exact futex
  address and opcode that previously returned `EOPNOTSUPP` now complete
  successfully.

This does not establish a complete deterministic Linux boot. Native QEMU boots
the generated initramfs to its marker and powers off. QEMU under deterministic
Hermit remains much slower and did not emit serial kernel output within the
bounded run. It remained alive until the host timeout rather than taking the
former glibc `SIGABRT` path.

## QEMU blocker evidence

The previous failure was:

```text
futex(0x555557231ea4, 0, -1, NULL, NULL, 0) = Err(Errno(EOPNOTSUPP))
```

A 20-second trace on PR #53 captured the same shared futex word succeeding:

```text
futex(0x555557231ea4, 1, 2147483647, NULL, NULL, 0) = Ok(1)
futex(0x555557231ea4, 0, -1, NULL, NULL, 0) = Ok(0)
```

Opcode `1` is `FUTEX_WAKE` and opcode `0` is `FUTEX_WAIT`, both without
`FUTEX_PRIVATE_FLAG`. The trace contained:

- 261,555 log lines
- 19,546 completed guest syscalls
- zero `EOPNOTSUPP` or `SIGABRT` matches

The longer QEMU run used a host-side 180-second timeout and exited 124 when the
timeout sent `SIGTERM`. There was no QEMU or glibc abort. The native control
boot reached:

```text
Run /init as init process
SHARED_FUTEX_QEMU_KERNEL_OK release=6.13.2-0_fbk15_hardened_0_g33ebba20e5e4 machine=x86_64
reboot: Power down
```

## Configuration

Hermit virtual time is enabled by default. There is no positive
`--virtualize-time` option, so the commands omit `--no-virtualize-time`.
`--no-virtualize-cpuid` avoids this host's lack of CPUID faulting and is
independent of futex behavior.

The optimized binary exercises release-mode syscall subscriptions, including
the new `munmap` and `mremap` tracking:

```bash
cargo build --release -p hermit --bin hermit
```

Generate transient binaries and the initramfs under ignored `target/`:

```bash
experiments/shared-futex-verify_20260722/build_assets.sh \
  target/shared-futex-verify_20260722
```

No generated executable, jar, initramfs, or full trace is committed. Artifact
hashes are recorded in `metadata.json`.

## Workload commands

Node.js:

```bash
timeout 90s target/release/hermit --log error run \
  --no-virtualize-cpuid -- \
  /usr/local/bin/node \
  experiments/shared-futex-verify_20260722/node_worker.js
```

Java:

```bash
timeout 60s target/release/hermit --log error run \
  --no-virtualize-cpuid -- \
  /usr/local/bin/java -jar \
  target/shared-futex-verify_20260722/threaded.jar
```

Pthreads:

```bash
timeout 30s target/release/hermit --log error run \
  --no-virtualize-cpuid -- \
  target/shared-futex-verify_20260722/pthread_futex
```

Native QEMU control:

```bash
timeout 30s qemu-system-x86_64 \
  -m 256M -accel tcg,thread=single -smp 1 \
  -kernel /boot/vmlinuz \
  -initrd target/shared-futex-verify_20260722/initramfs.cpio.gz \
  -display none -serial stdio -monitor none -no-reboot \
  -append 'console=ttyS0 panic=-1 rdinit=/init'
```

QEMU under Hermit:

```bash
timeout 180s target/release/hermit run --no-virtualize-cpuid -- \
  qemu-system-x86_64 \
  -m 256M -accel tcg,thread=single -smp 1 \
  -kernel /boot/vmlinuz \
  -initrd target/shared-futex-verify_20260722/initramfs.cpio.gz \
  -display none -serial stdio -monitor none -no-reboot \
  -append 'console=ttyS0 panic=-1 rdinit=/init'
```

Focused Detcore regression:

```bash
cargo test -p detcore --test tests_misc \
  shared_anonymous_futex_wakes_across_processes -- --nocapture
```

## Residual observations

- Default Node logging reports nondeterministic external-action warnings. The
  worker result is correct, but those warnings remain a separate
  reproducibility concern.
- Full Linux boot under deterministic Hermit remains limited by TCG-under-
  ptrace performance and possibly later virtual-time/TSC work. Shared futex is
  no longer the stopping condition.
- PR #53 supports `FUTEX_WAIT`, `FUTEX_WAKE`, and the existing bitset variants.
  This experiment does not claim support for PI, requeue, wake-op, or robust
  futex recovery modes.
