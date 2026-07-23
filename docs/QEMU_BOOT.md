# Booting Linux with QEMU under Hermit

Hermit can boot a minimal x86_64 Linux guest with QEMU's TCG accelerator. The
verified configuration reached the initramfs marker and powered off in 13.25
seconds. It combines Hermit's virtual time with QEMU's fixed instruction-count
clock.

This is a compatibility profile, not a fully deterministic VM profile.
`--no-sequentialize-threads` lets Linux schedule QEMU's host threads
concurrently, so their interleavings are not controlled by Hermit.

## Prerequisites

- An x86_64 Linux host.
- A release build of Hermit from a revision containing deterministic shared
  futex support.
- `qemu-system-x86_64` with TCG. The recorded run used QEMU 10.1.0.
- GCC, cpio, and gzip for the minimal initramfs.
- A readable x86_64 kernel image with initramfs and serial-console support.

Build Hermit:

```bash
cargo build --release -p hermit --bin hermit
```

On Debian or Ubuntu, the additional runtime tools are normally provided by:

```bash
sudo apt-get install -y qemu-system-x86 gcc cpio gzip
```

On Fedora or CentOS:

```bash
sudo dnf install -y qemu-system-x86-core gcc cpio gzip
```

## Quick smoke test

The smoke test compiles the minimal static `/init`, creates a gzip-compressed
initramfs, starts QEMU under Hermit, and requires the kernel marker before the
90-second host timeout:

```bash
./experiments/qemu-boot-debug/smoke_test.sh
```

It writes the initramfs and console log under `target/qemu-boot-smoke/`. Set
these environment variables when the defaults do not match the host:

```bash
KERNEL_IMAGE=/path/to/arch/x86/boot/bzImage \
QEMU_BIN=/path/to/qemu-system-x86_64 \
HERMIT_BIN=target/release/hermit \
QEMU_BOOT_TIMEOUT_SECONDS=90 \
  ./experiments/qemu-boot-debug/smoke_test.sh
```

The test passes only when QEMU exits successfully, the console contains
`SHARED_FUTEX_QEMU_KERNEL_OK`, and it contains none of the clock-calibration
failures observed in the control runs.

## Exact working command

After creating the initramfs as described below, the recorded working command
is:

```bash
timeout --signal=KILL 90s target/release/hermit --log error run \
  --no-sequentialize-threads \
  --preemption-timeout disabled \
  --no-virtualize-cpuid -- \
  qemu-system-x86_64 \
  -m 256M \
  -accel tcg,thread=single \
  -smp 1 \
  -icount shift=0,sleep=off \
  -kernel /boot/vmlinuz \
  -initrd target/qemu-boot-smoke/initramfs.cpio.gz \
  -display none \
  -serial stdio \
  -monitor none \
  -no-reboot \
  -append 'console=ttyS0 panic=-1 rdinit=/init'
```

`--no-virtualize-cpuid` was required on the evidence host because it did not
provide usable CPUID faulting. It exposes host CPUID results and is separate
from the scheduling and clock configuration. A host on which Hermit's CPUID
virtualization works may omit this option.

## Why QEMU needs concurrent host threads

Hermit normally serializes all threads and uses PMU retired-conditional-branch
preemption to choose among them deterministically. QEMU has a CPU-bound TCG
vCPU thread plus main-loop and helper threads that must service timers, I/O,
and wakeups.

With normal Hermit scheduling, a bounded QEMU boot made too little progress to
reach firmware serial output. Disabling virtual time did not fix that result.
Disabling only PMU preemption while keeping thread sequentialization let the
TCG vCPU starve QEMU's other threads.

The working profile therefore uses both:

- `--no-sequentialize-threads`, so QEMU's host threads can run concurrently;
- `--preemption-timeout disabled`, so Hermit does not apply PMU preemption to
  this compatibility run.

This restores boot throughput at the cost of deterministic QEMU host-thread
scheduling. `-accel tcg,thread=single -smp 1` still keeps the emulated guest
to one TCG vCPU; it does not serialize QEMU's host-side support threads.

## Why fixed QEMU icount is required

Without `-icount`, QEMU obtains guest TSC and device time from different host
clock paths. Under Hermit, the emulated TSC ultimately observes a thread-local
synthetic RDTSC value, while PIT, APIC, and PM timers observe virtualized
`CLOCK_MONOTONIC` aggregated across QEMU threads. Linux compares those clock
domains while calibrating its clocksource.

The no-icount control reached the kernel console but reported PIT calibration
failure, a 374 ms TSC watchdog skew, and finally:

```text
clocksource: No current clocksource.
```

`-icount shift=0,sleep=off` makes QEMU use one instruction-derived virtual
clock for guest TSC and device timers:

- `shift=0` advances QEMU virtual time by one nanosecond per guest
  instruction;
- `sleep=off` disables pacing that clock against host wall time.

The verified boot calibrated a coherent 1000.031 MHz TSC and emitted none of
the PIT, watchdog-skew, or no-clocksource warnings.

## Host time virtualization and clock calibration (issue #6)

Even without `-icount`, the QEMU-side symptom above has a Hermit-side cause and
a Hermit-side workaround.

By default Hermit virtualizes the guest's clocks, but it does so from **two
independent logical-time bases that are not coordinated with each other**:

- `rdtsc` is answered from a synthetic per-thread counter
  (`detcore::handle_rdtsc_event`);
- `clock_gettime` (and `gettimeofday`) return Hermit's virtual logical time
  regardless of the requested clock id (`detcore::handle_clock_gettime`).

QEMU derives the emulated TSC from the host `rdtsc`, but derives the emulated
PIT, PM timer, APIC timer, and RTC from host `clock_gettime`. Because those two
Hermit time bases advance independently — and, under
`--no-sequentialize-threads`, are per-thread and not globally coherent — the
nested Linux guest compares mutually inconsistent clock domains during
calibration and fails:

```text
tsc: Unable to calibrate against PIT
tsc: using PMTIMER reference calibration
clocksource: timekeeping watchdog ... 'tsc-early' skewed ... ns
clocksource: No current clocksource.
tsc: Marking TSC unstable due to clocksource watchdog
```

There are two independent ways to avoid this:

1. **QEMU side (deterministic-friendly):** `-icount shift=0,sleep=off`, which
   makes QEMU drive both the guest TSC and the emulated device timers from one
   instruction-derived virtual clock, as used by the verified profile above.
2. **Hermit side:** `--no-virtualize-time --no-virtualize-metadata`, which lets
   QEMU read the real, mutually consistent host clocks. This sacrifices time
   determinism for the whole run but calibrates normally and reaches the
   expected boot outcome.

To surface this, `hermit run` prints a one-line advisory when it launches a
`qemu-system-*` program while virtual time is enabled, pointing at both
workarounds. The advisory is informational only; it does not change behavior.

A fully coherent multi-clock model (a single Hermit time base shared by
`rdtsc`, `clock_gettime`, and their derived clocks, coordinated across threads)
would remove the need for either workaround but is out of scope here; this
section documents the supported workarounds instead.

## Kernel and initramfs

The smoke test defaults to `/boot/vmlinuz`. A distribution kernel is suitable
when it supports x86_64, gzip-compressed initramfs images, and the 8250 serial
console. The evidence run used:

```text
/boot/vmlinuz-6.13.2-0_fbk15_hardened_0_g33ebba20e5e4
```

To build a small kernel from a Linux source tree:

```bash
make x86_64_defconfig
scripts/config --enable BLK_DEV_INITRD
scripts/config --enable RD_GZIP
scripts/config --enable SERIAL_8250
scripts/config --enable SERIAL_8250_CONSOLE
make olddefconfig
make -j"$(nproc)" bzImage
export KERNEL_IMAGE="$PWD/arch/x86/boot/bzImage"
```

The smoke-test initramfs contains one freestanding static executable. Build it
manually from the repository root with:

```bash
out=target/qemu-boot-smoke
mkdir -p "$out/initramfs-root"
gcc -Os -nostdlib -static -fno-stack-protector -fno-pie -no-pie \
  experiments/shared-futex-verify_20260722/qemu_init.c \
  -o "$out/initramfs-root/init"
(
  cd "$out/initramfs-root"
  printf '.\n./init\n' | cpio --quiet -o -H newc
) | gzip -9 >"$out/initramfs.cpio.gz"
```

The init program prints the kernel release and architecture, syncs, and invokes
the Linux reboot syscall with `LINUX_REBOOT_CMD_POWER_OFF`. The expected end
of the serial log is:

```text
SHARED_FUTEX_QEMU_KERNEL_OK release=<kernel-release> machine=x86_64
reboot: Power down
```

## Troubleshooting

- **No serial output before the timeout:** Confirm both Hermit scheduling
  options are present. Default sequentialization is functionally live but was
  too slow for the bounded boot.
- **PIT calibration or TSC watchdog errors:** Confirm the exact
  `-icount shift=0,sleep=off` option. Do not replace it with host-clock
  pacing.
- **CPUID faulting error:** Retain `--no-virtualize-cpuid`. This makes CPUID
  host-dependent but does not disable virtual time.
- **Immediate QEMU futex rejection:** Use a Hermit revision containing
  deterministic process-shared futex support.
- **Timeout cleanup:** Keep `timeout --signal=KILL`; a sequentialized negative
  control may not process `SIGTERM` while a tracee is stopped.

## Evidence

The preserved experiment in [`experiments/qemu-boot-debug/`](../experiments/qemu-boot-debug/)
contains the six-mode comparison, exact host and binary metadata, timing, and
clock diagnostics. The successful row is
`virtual_minimal_fixed_icount` in [`results.csv`](../experiments/qemu-boot-debug/results.csv).
Large raw traces and console logs are intentionally excluded.
