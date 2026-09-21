# Booting Linux with QEMU under Hermit

Hermit can boot a minimal x86_64 Linux guest with QEMU's TCG accelerator in
two modes:

- The strict, sequentialized profile passed Stripped verification, not L2. The harness boots to
  the initramfs marker and powers off once as an oracle, then repeats that
  exact boot twice under `--strict --verify` and compares Detcore logs after
  selected numeric, address, path, and time fields are stripped.
- A faster compatibility profile reached the same marker in 13.25 seconds. It
  uses `--no-sequentialize-threads`, so QEMU's host-thread interleavings are not
  controlled by Hermit.

Both profiles combine Hermit's virtual time with QEMU's fixed
instruction-count clock. The strict result depends on deterministic `ppoll`
simulation, which lets QEMU's main thread wait while its vCPU and helper
threads run under Hermit's serialized scheduler.

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
./tests/qemu-boot/smoke_test.sh
```

It writes the initramfs and console log under `target/qemu-boot-smoke/`. Set
these environment variables when the defaults do not match the host:

```bash
KERNEL_IMAGE=/path/to/arch/x86/boot/bzImage \
QEMU_BIN=/path/to/qemu-system-x86_64 \
HERMIT_BIN=target/release/hermit \
QEMU_BOOT_TIMEOUT_SECONDS=90 \
  ./tests/qemu-boot/smoke_test.sh
```

The test passes only when QEMU exits successfully, the console contains
`SHARED_FUTEX_QEMU_KERNEL_OK`, and it contains none of the clock-calibration
failures observed in the control runs.

## Strict boot

Build the initramfs as described below, then use the normal strict scheduler
with a wall-clock bound long enough to reach the first serial output. The
recorded run first wrote to the serial console after 85.074 seconds and exited
after 166.486 seconds:

```bash
timeout --kill-after=10s --signal=TERM 180s \
  target/release/hermit --log info run --strict -- \
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

This command uses the ptrace backend, INFO logging, and no relaxations. A
successful exit and marker establish L1. Use the bounded harness for Stripped
two-run verification; it also rejects the known clock-calibration failures and
gives each verifier phase its own timeout. The environment variable and script
retain historical `L2` names, but bare `--verify` does not establish L2:

```bash
env HERMIT_BIN="$PWD/target/release/hermit" \
    KERNEL_IMAGE=/boot/vmlinuz \
    QEMU_BIN=/usr/local/bin/qemu-system-x86_64 \
    QEMU_L2_PHASE_TIMEOUT_SECONDS=360 \
    bash tests/qemu-boot/strict_l2_test.sh
```

The harness runs the same QEMU command shown above, first with `run --strict`
to require `SHARED_FUTEX_QEMU_KERNEL_OK`, then with
`run --strict --verify`. A 2026-07-28 run on QEMU 10.1.0 and Linux 6.17.13
compared 516137 messages per verifier run, including 459588 Detcore messages
and 363693 DETLOG/scheduler COMMIT messages after Stripped normalization. It
found no substantive differences and reported the harness's historical marker:

```text
:: Success: deterministic. Determinism verified.
QEMU strict L2 boot passed.
```

That marker records a Stripped pass; it is not canonical L2 evidence.

Do not add `--no-sequentialize-threads` or disable preemption when evaluating
the strict profile. Those options select the compatibility profile below.

The source-revisioned trace analysis is preserved in the parent workspace's
[`STRICT_BOOT_20260723.md`](https://github.com/rrnewton/dev-hermit/blob/main/experiments/hermit-experiments-migration_20260727/qemu-boot-debug/STRICT_BOOT_20260723.md).

## Fast compatibility command

After creating the initramfs as described below, the recorded working command
is:

```bash
timeout --signal=KILL 90s target/release/hermit --log error run \
  --no-sequentialize-threads \
  --max-timeslice disabled \
  --no-virtualize-cpuid -- \
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

`--no-virtualize-cpuid` was required on the evidence host because it did not
provide usable CPUID faulting. It exposes host CPUID results and is separate
from the scheduling and clock configuration. A host on which Hermit's CPUID
virtualization works may omit this option.

Both profiles use `-nodefaults -nic none` to omit QEMU's unused default
peripherals and network interface. The serial console remains explicit, and
`-rtc base=utc,clock=vm` keeps the RTC on QEMU's instruction-derived VM clock.

## Scheduling profiles

Hermit normally serializes all threads and uses PMU retired-conditional-branch
preemption to choose among them deterministically. QEMU has a CPU-bound TCG
vCPU thread plus main-loop and helper threads that must service timers, I/O,
and wakeups.

Before `ppoll` was determinized, a 30-minute strict run produced no serial
output and advanced only 0.830 seconds of Hermit virtual CPU time. The QEMU
main thread could enter `ppoll` without a deterministic simulated wait, so the
serialized scheduler did not reliably hand execution to the vCPU/helper
threads.

Current main intercepts `ppoll`, probes it nonblocking, and waits through the
deterministic I/O scheduler. In the successful strict trace, all 23 `ppoll`
calls completed and the vCPU owned 827 of 980 visible COMMIT records. This made
the default strict scheduler sufficient; no concurrency or preemption
relaxation was used.

The faster compatibility profile still uses both:

- `--no-sequentialize-threads`, so QEMU's host threads can run concurrently;
- `--max-timeslice disabled`, so Hermit does not apply PMU preemption to
  this compatibility run.

That profile trades deterministic QEMU host-thread scheduling for lower wall
time. `-accel tcg,thread=single -smp 1` keeps the emulated guest to one TCG
vCPU in either profile; it does not remove QEMU's host-side support threads.

## What `-icount` does, and when a boot needs it

`-icount shift=0,sleep=off` makes QEMU drive the guest TSC and the emulated
device timers from one instruction-derived virtual clock:

- `shift=0` advances QEMU virtual time by one nanosecond per guest
  instruction;
- `sleep=off` disables pacing that clock against host wall time.

That removes the clock-domain mismatch described in the next section, and the
verified strict boot calibrates a coherent 1000.031 MHz TSC with none of the
PIT, watchdog-skew, or no-clocksource warnings.

An earlier revision of this document titled this section "Why fixed QEMU icount
is required" and presented `-icount` as a precondition for booting at all.
**That is not what the measurements show, and the claim is retracted.** Whether
a boot needs `-icount` depends on the profile and on the guest command line,
and `-icount` carries two costs that the old framing hid.

### Measured: `-icount` is not required, and is not free

Measured 2026-08-18 at hermit `770b95c505` (binary SHA-256 `4d8e8924…`), QEMU
10.1.2, guest 6.17.13, BusyBox initramfs. PASS means the guest reached
the run's success marker.

| profile | `-icount` | vCPUs | guest cmdline | wall | outcome |
| --- | --- | ---: | --- | ---: | --- |
| compatibility | off | 1 | default | 19.3 s | PASS |
| compatibility | off | 2 | default | 23.4 s | PASS |
| compatibility | on | 1 | default | 31.8 s | PASS |
| strict | off | 1 | default | 59.4 s | fails, guest panic |
| strict | off | 2 | default | 60.6 s | fails, guest panic |
| strict | on | 1 | default | 151.8 s | PASS |
| strict | on | 2 | default | 0.1 s | QEMU refuses to start |
| strict | off | 1 | `no_timer_check` | 103.5 s | PASS, TSC 999.964 MHz |
| strict | off | 2 | `no_timer_check lpj=999964` | 382.0 s | PASS |

Three things follow, none of them consistent with "required":

1. **In the compatibility profile, no-icount boots and is the faster option.**
   19.3 s without `-icount` against 31.8 s with it, at one vCPU. Turning
   `-icount` on cost about 1.6x here.
2. **In the strict profile the default-cmdline no-icount failure is real**, and
   it is the panic documented in the next section. But it is a *guest timer
   setup* failure, not an inability to run QEMU: adding `no_timer_check` to the
   guest command line reaches PASS without `-icount` at all.
3. **`-icount` is what produces the zero-output case, not no-icount.** The one
   cell above that emits no console bytes is `-icount` with two vCPUs, and the
   cause is QEMU declining the combination outright — see below.

### `-icount` forecloses multi-threaded TCG, by QEMU's own rule

This is the mechanical constraint the document never stated, and it is a
property of QEMU rather than a Hermit limitation. QEMU refuses the combination:

```text
qemu-system-x86_64: -accel tcg,thread=multi: No MTTCG when icount is enabled
```

So enabling `-icount` forces `thread=single`, collapsing every guest vCPU onto
one host thread. A reader who takes `-icount` as mandatory would reasonably
conclude that steering the interleaving of two guest vCPUs is impossible under
Hermit. It is not impossible; it is incompatible *with `-icount`*. The strict,
no-icount, two-vCPU row above reaches PASS with both vCPUs on separate host
threads, which is the configuration in which interleaving can be steered at
all.

### An unreconciled report

A separate report described no-icount stalling with zero output. Nothing in the
matrix above reproduces that: every no-icount cell produced console output —
9,233 bytes even in the failing strict rows — and the only zero-output cell is
`-icount` at two vCPUs, which is QEMU's MTTCG refusal above. That report may
have used a configuration not covered here, or may have attributed the
`-icount` two-vCPU case to no-icount. Treat the shape of a no-icount failure as
configuration-dependent, and record the profile, vCPU count and guest command
line alongside any future result.


## Host time virtualization and clock calibration (issue #6)

The QEMU-side symptom above has a Hermit-side cause. It does not currently have
a working Hermit-side workaround; see the measurements later in this section.

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

Of the two conceivable ways out, only the QEMU-side one works as written:

1. **QEMU side (works):** `-icount shift=0,sleep=off`,
   which makes QEMU drive both the guest TSC and the emulated device timers from
   one instruction-derived virtual clock, as used by the verified profile above.
2. **Hermit side (does *not* work):** `--no-virtualize-time
   --no-virtualize-metadata` is intended to let QEMU read the real, mutually
   consistent host clocks. It does not rescue a boot that lacks `-icount`.

An earlier revision of this section claimed the Hermit-side option "calibrates
normally and reaches the expected boot outcome". That claim was wrong. Measured
on 2026-08-18 with the canonical command above, changing only `-icount` and the
Hermit time flags, and using this document's own oracle
(`SHARED_FUTEX_QEMU_KERNEL_OK` followed by `reboot: Power down`):

| kernel | `-icount` | Hermit time | `--strict` | outcome |
| --- | --- | --- | --- | --- |
| 6.19.2 | yes | virtualized | yes | boot OK, 110 s |
| 6.13.2 | yes | virtualized | yes | boot OK, 109 s |
| 6.19.2 | no | virtualized | yes | guest panic after 8,953 serial bytes |
| 6.19.2 | no | `--no-virtualize-time` | yes | Hermit exits 1 immediately |
| 6.19.2 | no | `--no-virtualize-time` | no | guest panic, same signature |
| 6.13.2 | no | `--no-virtualize-time` | no | guest panic, same signature |

`6.13.2` is `6.13.2-0_fbk15_hardened_0_g33ebba20e5e4`, the kernel named in the
Evidence section below, so this is not a newer-kernel regression. The two
`-icount` rows establish that the reproduction is faithful: the supported route
boots cleanly on both kernels on the same host.

The failure is a guest panic during timer setup, not the softer calibration
degradation described above:

```text
..MP-BIOS bug: 8254 timer not connected to IO-APIC
Kernel panic - not syncing: IO-APIC + timer doesn't work!
```

Two further points. The option is *inert* against this failure: the panic
signature is identical with and without it, so it changes nothing about the
outcome. And it is incompatible with `--strict`, which the canonical command
uses — strict mode rejects the now-unvirtualized `gettimeofday`, so Hermit exits
before the guest starts:

```text
ERROR detcore: [detcore, dtid 3] inbound syscall: gettimeofday(...) = ?
Error: Sandbox container exited unexpectedly
```

So `-icount shift=0,sleep=off` is the one *Hermit-flag-free* way to make the
canonical strict command boot as written. It is not the only way to boot
without `-icount` at all: as the matrix earlier in this document shows, the
compatibility profile boots without it, and the strict profile boots without it
once the guest is given `no_timer_check`. Choose `-icount` when you want the
canonical command to work unchanged; avoid it when you need more than one TCG
thread, which it forbids.

`hermit run` prints a one-line advisory when it launches a
`qemu-system-*` program while virtual time is enabled. It recommends the two
measured routes: `-icount shift=0,sleep=off`, or `no_timer_check` on the nested
guest kernel command line. It does not recommend the ineffective Hermit time
flags. The advisory is informational only; it does not change behavior.

A fully coherent multi-clock model (a single Hermit time base shared by
`rdtsc`, `clock_gettime`, and their derived clocks, coordinated across threads)
would remove the need for `-icount` here, and is the real fix for the
Hermit-side cause described in this section. It remains out of scope for this
document, which records what is measured to work today.

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
  tests/shared-futex-verify/qemu_init.c \
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

- **No serial output before the timeout:** A strict INFO run first emitted
  serial data after 85 seconds on the evidence host, so use at least a
  180-second bound. For the fast compatibility profile, confirm both Hermit
  scheduling relaxations are present.
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

The preserved experiment in the parent workspace's
[`qemu-boot-debug/`](https://github.com/rrnewton/dev-hermit/tree/main/experiments/hermit-experiments-migration_20260727/qemu-boot-debug) contains the
original six-mode comparison plus the strict current-main follow-up. The fast
compatibility row is `virtual_minimal_fixed_icount`; the original strict L1 row is
`strict_current_main_ppoll` in
[`results.csv`](https://github.com/rrnewton/dev-hermit/blob/main/experiments/hermit-experiments-migration_20260727/qemu-boot-debug/results.csv). Large raw traces
and console logs are intentionally excluded.

The source-revisioned, historically named
[`qemu_strict_l2_boot_20260727`](https://github.com/rrnewton/dev-hermit/tree/main/experiments/qemu_strict_l2_boot_20260727)
experiment records the first successful strict Stripped run, including the exact
Hermit and Reverie revisions, kernel and QEMU versions, guest command, boot
oracle, and verifier comparison counts. Its directory name predates the
Stripped-versus-L2 distinction.
