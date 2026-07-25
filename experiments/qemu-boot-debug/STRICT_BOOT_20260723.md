# Strict QEMU boot on current main

Date: 2026-07-23 PDT / 2026-07-24 UTC

Repository: `rrnewton/hermit`
Revision: `dd60278fc3c20f102442f26bb02b98a35e7246e3`
Branch under test: `qemu-boot-debug-overnight-slot47`
Host: `devbig030.atn3.facebook.com`

## Result

QEMU 10.1.0 booted Linux 6.17.13 under literal Hermit `run --strict`, ran the
minimal `/init`, and powered off with exit status 0. The run used the ptrace
backend, INFO logging, and no determinism relaxations.

```text
[    0.000000] Linux version 6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79 ...
[    1.300205] Run /init as init process
SHARED_FUTEX_QEMU_KERNEL_OK release=6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79 machine=x86_64
[    1.300519] reboot: Power down
```

This establishes L1 assurance. `--verify` was not run, so this report does not
claim an L2 bitwise-identical repeat.

## Reproduction

Build Hermit and the checked-in freestanding initramfs:

```bash
with-proxy cargo build --release -p hermit
with-proxy ./experiments/shared-futex-verify_20260722/build_assets.sh \
  target/qemu-boot-overnight
```

Run the strict profile:

```bash
with-proxy timeout --kill-after=10s --signal=TERM 180s \
  target/release/hermit --log info run --strict -- \
  qemu-system-x86_64 \
  -m 256M \
  -accel tcg,thread=single \
  -smp 1 \
  -icount shift=0,sleep=off \
  -kernel /boot/vmlinuz \
  -initrd target/qemu-boot-overnight/initramfs.cpio.gz \
  -display none \
  -serial stdio \
  -monitor none \
  -no-reboot \
  -append 'console=ttyS0 panic=-1 rdinit=/init' \
  > /tmp/qemu-overnight-slot47.console.log \
  2> /tmp/qemu-overnight-slot47.info.log
```

Exact inputs:

| Input | Value |
| --- | --- |
| Hermit revision | `dd60278fc3c20f102442f26bb02b98a35e7246e3` |
| Hermit binary SHA-256 | `1f49c621dc8e7de7559a6637510e8c83d7a56bd46a81bfdae1a47abbdc649163` |
| Kernel SHA-256 | `e4b1c0248a31c7e1f7cb31d82a1a03d4e7cab408ee1b8e622dd897c17eae46a2` |
| Initramfs SHA-256 | `f88ddaba3fa86a44078d550f92e13f0d23e5a1f0a983aadb24e678e2ef5523cc` |
| QEMU | `10.1.0 (qemu-kvm-10.1.0-21.el9)` |
| Host kernel | `6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79 x86_64` |

## Observations

| Measurement | Value |
| --- | ---: |
| Wall time from first to last INFO timestamp | 166.486 s |
| First serial write | 85.074 s |
| Exit status | 0 |
| Console | 311 lines, 21,626 bytes |
| Reported scheduler turns | 987 |
| Visible COMMIT records | 980 |
| Hermit virtual CPU time | 165.226467225 s |
| Completed syscalls | 167,521 |
| Clock-calibration failure matches | 0 |
| Hermit ERROR/panic/unsupported matches | 0 |

The sole Hermit warning was a deterministic CPUID-table miss for leaf
`0x8000000`; Detcore returned zero and the boot continued. The two
`io_uring_setup` calls returned the intentional `ENOSYS` policy, after which
QEMU fell back to its non-io_uring path.

## Syscall diagnosis

No missing syscall handler blocks the current strict boot. The dominant calls
and their exhaustive classifications were:

| Syscall | Count | Classification | Interpretation |
| --- | ---: | --- | --- |
| `gettimeofday` | 141,813 | Determinized | QEMU TCG virtual-clock hot path; high volume, not a failed wait. |
| `writev` | 21,626 | Unclassified passthrough | One-byte serial writes; output completed, but policy needs L2 review. |
| `write` | 1,634 | Determinized | Eventfd and ordinary output writes completed. |
| `mprotect` | 635 | Pass-through | Explicitly accepted under container/serialization assumptions. |
| `madvise` | 509 | Unclassified passthrough | Completed; not a boot blocker, but policy needs review. |
| `futex` | 93 | Determinized | 77 returned `Ok(0)` and 16 returned `Ok(1)`. |
| `ppoll` | 23 | Determinized | 2 returned 0, 19 returned 1, and 2 returned 2. |
| `io_uring_setup` | 2 | Determinized refusal | Both returned `ENOSYS`; QEMU used its fallback. |

The inbound/finished totals differ by three because normal group shutdown does
not produce completion records for `exit_group(0)` or for the two worker
threads' final sleeping futex waits. The main thread woke those workers and
Hermit removed them during the authorized group exit. This is not an
unmatched-wake deadlock.

### Why `ppoll` was the blocker

The earlier 30-minute strict baseline, before deterministic `ppoll` handling,
timed out with zero console bytes after only 0.830167895 seconds of Hermit
virtual CPU progress. Its main thread stayed in the
`clock_gettime`/`gettimeofday`/`ppoll` loop while helper and vCPU threads made
too little progress.

Current main classifies `ppoll` as determinized, converts it to nonblocking
probes, and waits through Detcore's deterministic I/O scheduler. The long
`ppoll` spans in this trace are healthy handoffs: while the main thread waited,
the vCPU ran. For example, one `ppoll` entered at `03:13:43.559457Z` and
completed at `03:13:59.383828Z`; another remained pending for 44.542 seconds.
The vCPU owned 827 of 980 visible COMMIT records and produced the serial boot.

The remaining long syscall-free gaps are TCG execution between PMU
preemptions, not blocked host syscalls. The longest pre-shutdown gap was 6.764
seconds between an eventfd `write` and the next `gettimeofday`. After the final
serial write, QEMU spent 26.827 seconds in its normal poweroff/teardown path
before the main thread's `ppoll` completed and `exit_group(0)` ran.

## Follow-up

1. Run this profile with `--verify` to establish or reject L2 assurance.
2. Classify the observed `writev` and `madvise` passthroughs before treating an
   L2 result as broad syscall-policy coverage.
3. Add a long-timeout strict boot job that requires the marker and rejects the
   clock-calibration failure strings. Keep the existing short relaxed smoke
   test for fast compatibility feedback.
4. Reduce the one-byte `writev` and virtual-time interception overhead; INFO
   produced a 52,597,962-byte trace for this boot.

Raw logs are intentionally not committed. Their SHA-256 values are:

```text
d292138cab7f1874966a73e34e75cc563a86565c10ff830c8f575fbe177e23c8  console.log
ae8f1fd1b18606954a3d94f3aefdc1733948a406da08d8b2323582270a0a5aeb  info.log
```
