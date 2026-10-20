# Booting Linux with QEMU under Hermit

Hermit can run QEMU as a Linux program and boot a minimal x86_64 Linux kernel
inside it. These examples use QEMU's TCG software emulator, which translates
guest instructions instead of using hardware virtualization.

There are two useful starting points:

- **Strict boot:** Hermit controls the scheduling of QEMU's host threads. The
  command below boots one emulated CPU with Hermit's default ptrace backend
  and no scheduling relaxations.
- **Compatibility smoke test:** QEMU's host threads run concurrently, with
  Hermit's preemption disabled. This checks that the kernel reaches the test
  program and powers off; it does not establish repeatable thread scheduling.

A successful boot is not proof that two executions match. The distinction
between booting and comparing repeated runs is explained under
[Verification](#verification).

## Prerequisites

- An x86_64 Linux host and the [Hermit build dependencies](../README.md#requirements).
- A release build of Hermit.
- `qemu-system-x86_64` with TCG.
- GCC, cpio, and gzip for the minimal initramfs (the guest's initial filesystem).
- A readable x86_64 kernel image with initramfs and serial-console support.
- Access to the CPU performance counters used for Hermit's deterministic
  preemption when running the strict profile.

Run the commands in this guide from the Hermit repository root unless stated
otherwise. Build Hermit. This rebuilds `target/release/hermit`, which
`target/install_pkg/hermit` links to, with default features; if you built the
`dbt`, `sabre`, and `e9patch` backends from the README, use
`cargo build --release --workspace --features hermit/third-party-backends`
instead so the rebuild keeps them:

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

## Kernel and initramfs

The commands below use `/boot/vmlinuz`. Replace it with the path to a readable
kernel image on your machine. A distribution kernel is suitable when it
supports x86_64, gzip-compressed initramfs images, and the 8250 serial console.

To build a small kernel, run these commands **from a Linux source tree**:

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

The test initramfs contains one freestanding static executable. Build it
**from the Hermit repository root** with:

```bash
out=target/qemu-boot-smoke
mkdir -p "$out/initramfs-root"
gcc -Os -nostdlib -static -fno-stack-protector -fno-pie -no-pie \
  tests/shared-futex-verify/qemu_init.c \
  -o "$out/initramfs-root/init"
(
  cd "$out/initramfs-root"
  printf '.\n./init\n' | cpio --quiet -o -H newc
) | gzip -9 >"$out/initramfs.cpio.gz"
```

The [init program](../tests/shared-futex-verify/qemu_init.c) prints the kernel
release and architecture, syncs, and requests a power-off. A successful boot
ends with:

```text
SHARED_FUTEX_QEMU_KERNEL_OK release=<kernel-release> machine=x86_64
reboot: Power down
```

## Strict boot

After building the initramfs, run:

```bash
timeout --kill-after=10s --signal=TERM 180s \
  target/release/hermit --log error run --strict -- \
  qemu-system-x86_64 \
  -nodefaults \
  -nic none \
  -m 256M \
  -accel tcg,thread=single \
  -smp 1 \
  -icount shift=0,sleep=off \
  -rtc base=utc,clock=vm \
  -kernel /boot/vmlinuz \
  -initrd target/qemu-boot-smoke/initramfs.cpio.gz \
  -display none \
  -serial stdio \
  -monitor none \
  -no-reboot \
  -append 'console=ttyS0 panic=-1 rdinit=/init'
```

This uses the ptrace backend, error-level logging, and no determinism
relaxations. Error-level logging keeps Hermit's verbose execution trace out of
the terminal; the guest's serial output remains visible. A boot can take
minutes, depending on the host and kernel. The timeout terminates the command
after 180 seconds and escalates to a kill after another 10 seconds.

Require both a successful command exit and the marker shown above. A kernel
panic or timeout is a failed boot, even if QEMU itself exits successfully.
This is a single boot check, not a comparison of repeated executions.

Do not add `--no-sequentialize-threads` or disable preemption when evaluating
the strict profile. Those changes select the compatibility profile instead.

## Compatibility smoke test

The [smoke-test script](../tests/qemu-boot/smoke_test.sh) builds its own
initramfs, starts QEMU under Hermit, and checks the kernel marker:

```bash
./tests/qemu-boot/smoke_test.sh
```

It writes the initramfs and console log under `target/qemu-boot-smoke/`. It
finds QEMU on `PATH`. Set these environment variables when the defaults do not
match the host:

```bash
KERNEL_IMAGE=/path/to/arch/x86/boot/bzImage \
QEMU_BIN=/path/to/qemu-system-x86_64 \
HERMIT_BIN=target/release/hermit \
QEMU_BOOT_TIMEOUT_SECONDS=90 \
  ./tests/qemu-boot/smoke_test.sh
```

The script requires QEMU to exit successfully within the host timeout, the
console to contain `SHARED_FUTEX_QEMU_KERNEL_OK`, and no known guest
clock-calibration failures. It uses three relaxations:

- `--no-sequentialize-threads` allows QEMU's host threads to run concurrently.
- `--max-timeslice disabled` disables Hermit's performance-counter preemption.
- `--no-virtualize-cpuid` exposes the host's CPU feature information. This
  supports hosts without usable CPUID interception, but makes those results
  host-dependent.

The smoke test is a compatibility check, not evidence of deterministic
execution. Its single emulated CPU does not mean QEMU has only one host thread:
QEMU still needs main-loop and helper threads for timers, I/O, and wakeups.

## Verification

The public [repeat-boot harness](../tests/qemu-boot/strict_l2_test.sh) first
checks the boot marker, then runs the same guest twice with `--strict --verify`.
It requires a `verification-report` executable alongside Hermit, or an explicit
`VERIFICATION_REPORT_BIN` path. It finds QEMU on `PATH` unless `QEMU_BIN` is set.
It retains verbose logs under `target/qemu-strict-l2/` and applies a separate
timeout to each phase; these logs can be large.

The harness's filename and success message contain the historical label
**L2**, but its current command uses the default **Stripped** comparison. That
comparison can erase numeric values, addresses, paths, and times from selected
log messages before comparing them. A match therefore does not establish that
all observed values repeated.

In older reports, **L1** denotes a successful strict run; **L2** denotes a
stronger comparison of repeated runs, including program output and Hermit's
canonical execution log. A single boot reaches only the first of those checks.
For the current strict comparison options and their limits, see
[Verify Mode in the user guide](USER_GUIDE.md#verify-mode). Do not use the
repeat-boot harness's historical success message as a strict-comparison result.

## Clocks and QEMU options

`-icount shift=0,sleep=off` gives the emulated CPU and device timers one clock
derived from the number of guest instructions executed:

- `shift=0` advances QEMU virtual time by one nanosecond per guest instruction.
- `sleep=off` disables pacing against host wall time.

The command also uses `-rtc base=utc,clock=vm` for the emulated real-time clock,
and `-nodefaults -nic none` to omit unused default devices and networking.

Without `-icount`, QEMU can observe a mismatch between Hermit's synthetic CPU
timestamp counter (`RDTSC`) and the time returned by `clock_gettime`. The Linux
kernel inside QEMU compares clocks during boot and may report timer calibration
errors or panic:

```text
..MP-BIOS bug: 8254 timer not connected to IO-APIC
Kernel panic - not syncing: IO-APIC + timer doesn't work!
```

`-icount` is the starting point for the strict command above, not a universal
requirement for booting Linux. Compatibility boots without it have succeeded.
Strict boots without it have also succeeded with `no_timer_check` on the
**guest kernel command line**, bypassing the kernel's timer check. That does
not repair the disagreement between clocks. Disabling Hermit's time
virtualization is not a supported remedy for this failure and is incompatible
with the strict command above.

There is also a QEMU limitation: instruction-count timing cannot be combined
with multi-threaded TCG. QEMU refuses that combination with:

```text
qemu-system-x86_64: -accel tcg,thread=multi: No MTTCG when icount is enabled
```

The example therefore uses `thread=single` and one emulated CPU. Experiments
with multiple emulated CPUs and separate host threads need a different clock
configuration; a successful single-CPU boot does not establish that case.

## Troubleshooting

- **No serial output before the timeout:** Strict boot can spend considerable
  host time before the first serial output. Check the kernel path and required
  kernel features, then choose a longer bound if needed.
- **Timer calibration or clocksource errors:** Check that the exact
  `-icount shift=0,sleep=off` option is present. See the clock discussion above;
  adding `--no-virtualize-time` is not a fix.
- **CPUID interception unavailable:** The compatibility smoke test disables
  this interception. Adding `--no-virtualize-cpuid` to another command makes
  CPU feature information host-dependent; report that relaxation explicitly.
- **Performance counters unavailable:** The strict profile needs Hermit's
  preemption support. The compatibility smoke test disables preemption, but
  its result does not establish deterministic scheduling.
- **Large logs:** Keep `--log error` for an interactive boot check. The
  repeat-boot harness intentionally collects verbose logs; bound their disk
  use when running it.
