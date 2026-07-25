# QEMU virtual-time boot diagnosis

See [the QEMU boot guide](../../docs/QEMU_BOOT.md) for the maintained
configuration and `smoke_test.sh` for the automated marker and clock-failure
checks.

> **2026-07-23 update:** Current main boots this workload under literal
> `run --strict` with no relaxations. The source-revisioned L1 result and
> syscall analysis are in
> [`STRICT_BOOT_20260723.md`](STRICT_BOOT_20260723.md). The report below is the
> earlier baseline that established the fast compatibility profile and the
> pre-`ppoll` strict failure mode.

Date: 2026-07-22 UTC (2026-07-21 PDT)

Repository: `rrnewton/hermit`
Branch under test: `impl-shared-futex-support-slot09`
Hermit head before this report: `a40bf685ab5cddce77c7ce81209a5577bc8f83fc`
Shared-futex implementation: `b44d9396418c316abdbc38e9e41e5809da936c49`

## Result

QEMU does boot successfully with Hermit virtual time after the shared-futex fix.
The working run reached the initramfs marker and powered off in 13.25 seconds:

```text
SHARED_FUTEX_QEMU_KERNEL_OK release=6.13.2-0_fbk15_hardened_0_g33ebba20e5e4 machine=x86_64
reboot: Power down
```

The original no-serial symptom is not another shared-futex failure. It is the
combined effect of two independent constraints:

1. Hermit's deterministic thread sequentialization and PMU preemption make
   QEMU TCG progress too slowly for the bounded boot. Disabling time
   virtualization alone does not fix this.
2. Without QEMU `icount`, Hermit exposes incompatible clock domains to QEMU:
   emulated guest TSC derives from a thread-local synthetic RDTSC, while PIT,
   APIC, and PM timers derive from global `CLOCK_MONOTONIC`. The kernel detects
   the resulting skew and loses its clocksource.

The successful operational configuration disables Hermit thread
sequentialization and PMU preemption, while QEMU fixed `icount` supplies one
instruction-derived clock to guest TSC and device timers.

This is a working virtual-time boot configuration, but it is not a claim of
fully deterministic QEMU process scheduling. `--no-sequentialize-threads`
allows the QEMU host threads to run concurrently.

## Experiment matrix

| Mode | Hermit scheduling | QEMU clock | Bound/result | Serial progress |
| --- | --- | --- | --- | --- |
| Default virtual time | sequentialized, 200 ms virtual RCB preemption | host-derived | 180 s timeout | none |
| No virtual time | sequentialized, 200 ms virtual RCB preemption | host-derived | 90 s timeout | none |
| Virtual time, no sequentialization | concurrent, default RCB preemption | host-derived | 30 s timeout | SeaBIOS and option ROM |
| Virtual time, fixed icount | sequentialized, preemption disabled | `shift=0,sleep=off` | 30 s timeout; required forced cleanup | none |
| Virtual time, no icount | concurrent, preemption disabled | host-derived | 30 s timeout | kernel reaches clocksource failure |
| Virtual time, fixed icount | concurrent, preemption disabled | `shift=0,sleep=off` | exit 0 in 13.25 s | complete boot and poweroff |

The fourth row isolates sequentialization from PMU overhead. With
sequentialization retained and preemption disabled, the CPU-bound TCG vCPU
does not yield enough for the QEMU main and helper threads to make progress.
The host timeout's `SIGTERM` could not unwind this state, so only that
task-owned process tree was cleaned up with `SIGKILL`.

## Why default mode appears silent

A 20-second default virtual-time trace recorded:

- 261,555 trace lines
- 19,546 completed guest syscalls
- 120 deterministic scheduler turns
- 104 turns for QEMU main thread `dtid 3`
- 11 turns for helper thread `dtid 5`
- 5 turns for vCPU thread `dtid 7`
- 15,900 `clock_gettime` calls
- 2,879 `gettimeofday` calls
- only 574,359,370 ns of final virtual global CPU time

The final pre-timeout window is a dense main-thread time-polling loop. Its
`CLOCK_MONOTONIC` values advance by roughly 10 microseconds per intercepted
call. The vCPU and helper threads repeatedly appear at futex or sleep resource
boundaries. Shared `FUTEX_WAIT` and `FUTEX_WAKE` complete successfully; there
is no `EOPNOTSUPP` or abort.

A matched no-virtual-time run still produced no serial output in 90 seconds.
Its bounded trace completed only 719 syscalls and 124 scheduler turns. Time
reads used the host vDSO and therefore disappeared from Hermit's syscall log,
but TCG execution still remained below the firmware-console milestone. This
rules out virtual time as the sole cause of the silent boot.

Disabling thread sequentialization restored SeaBIOS output in the same bound.
Disabling both sequentialization and PMU preemption removed the throughput
collapse and made the fixed-icount boot complete in 13.25 seconds.

## Clock-domain failure without icount

The minimally intercepted no-icount control reached the Linux console quickly,
then printed:

```text
tsc: Unable to calibrate against PIT
tsc: using PMTIMER reference calibration
tsc: Detected 304.009 MHz processor
Clocksource 'tsc-early' skewed -374910756 ns (-374 ms) over watchdog
'refined-jiffies' interval of 495924608 ns (495 ms)
clocksource: No current clocksource.
tsc: Marking TSC unstable due to clocksource watchdog
```

Hermit intercepts both inputs; this is not an interception bypass:

- QEMU TCG's guest `RDTSC` helper ultimately executes host `RDTSC`.
- Reverie enables `PR_SET_TSC/PR_TSC_SIGSEGV` and Detcore handles the fault.
- QEMU PIT, APIC, and PM timers use `QEMU_CLOCK_VIRTUAL`, backed by
  `CLOCK_MONOTONIC`, which Detcore also virtualizes.
- Detcore currently returns RDTSC from the calling thread's local `DetTime`.
- `clock_gettime` publishes local progress and returns aggregated
  `GlobalTime` across QEMU threads.

QEMU compares the local-TSC and global-device-clock domains during Linux clock
calibration. They advance at different rates, which explains the measured
skew.

With `-icount shift=0,sleep=off`, QEMU routes both its elapsed-tick and virtual
device-clock functions through `icount_get`. Each guest instruction advances
the VM clock by one nanosecond, and idle time warps to timer deadlines without
host pacing. The successful run calibrated a coherent 1000.031 MHz TSC and
contained none of the PIT calibration, watchdog skew, or no-clocksource
warnings.

## Reproduction

Build the PR head and regenerate the initramfs from the sibling shared-futex
experiment:

```bash
cargo build --release -p hermit --bin hermit
experiments/shared-futex-verify_20260722/build_assets.sh \
  target/shared-futex-verify_20260722
```

Successful virtual-time boot:

```bash
timeout 90s target/release/hermit --log error run \
  --no-sequentialize-threads \
  --preemption-timeout disabled \
  --no-virtualize-cpuid -- \
  qemu-system-x86_64 \
  -m 256M \
  -accel tcg,thread=single \
  -smp 1 \
  -icount shift=0,sleep=off \
  -kernel /boot/vmlinuz \
  -initrd target/shared-futex-verify_20260722/initramfs.cpio.gz \
  -display none \
  -serial stdio \
  -monitor none \
  -no-reboot \
  -append 'console=ttyS0 panic=-1 rdinit=/init'
```

Matched clock-failure control: omit only QEMU's `-icount` option from the
successful command. It reaches the Linux console but loses its TSC clocksource.

Default-scheduler control: omit Hermit's `--no-sequentialize-threads` and
`--preemption-timeout disabled`. Use a hard host kill for a bounded negative
test because the stopped tracee may not process `SIGTERM`:

```bash
timeout --signal=KILL 30s target/release/hermit --log error run \
  --no-virtualize-cpuid -- \
  qemu-system-x86_64 \
  -m 256M \
  -accel tcg,thread=single \
  -smp 1 \
  -icount shift=0,sleep=off \
  -kernel /boot/vmlinuz \
  -initrd target/shared-futex-verify_20260722/initramfs.cpio.gz \
  -display none \
  -serial stdio \
  -monitor none \
  -no-reboot \
  -append 'console=ttyS0 panic=-1 rdinit=/init'
```

`--no-virtualize-cpuid` is required by this host's lack of CPUID faulting and
is unrelated to the QEMU clock diagnosis.

## Recommended follow-up

1. Treat fixed QEMU `icount` as the supported TCG virtual-clock configuration.
2. Make Detcore RDTSC observe the same global logical scalar as
   `clock_gettime` and define an explicit virtual TSC frequency, nominally
   1 GHz if cycles continue to equal nanoseconds.
3. Add a low-overhead global-time snapshot so frequent QEMU RDTSC reads do not
   require a scheduler RPC.
4. Investigate a deterministic QEMU scheduling profile that can run the TCG
   vCPU efficiently while servicing QEMU's main/helper threads. Disabling
   preemption while retaining sequentialization starves those threads;
   retaining default PMU preemption is functionally live but far too slow.
5. Add a QEMU boot regression that requires the initramfs marker and rejects
   PIT calibration failures, TSC watchdog skew, and no-clocksource warnings.

Full raw traces and console logs remain under ignored `target/qemu-boot-debug/`
and are intentionally not committed.
