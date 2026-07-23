# QEMU + Linux boot demo (under Hermit)

`demo.sh` boots an unmodified Linux kernel inside QEMU, with QEMU itself running
as a Hermit guest, runs a handful of ordinary programs inside the booted guest,
and powers the machine off cleanly (exit 0). It is self-contained and needs no
pre-staged artifacts.

## Run it

```bash
./experiments/qemu-linux/demo.sh
```

Expected: a full kernel boot log, then the guest programs, then a clean
power-off, ending with `RESULT: PASS` and exit status 0 (~15-25s wall time; the
guest itself boots in <2s). A captured run is in [`SAMPLE_OUTPUT.txt`](SAMPLE_OUTPUT.txt).

### Knobs (environment variables)

| Variable        | Default                        | Meaning                                   |
|-----------------|--------------------------------|-------------------------------------------|
| `HERMIT_BIN`    | `target/release/hermit` (or debug) | Hermit binary to use                  |
| `KERNEL`        | `/boot/vmlinuz`                | kernel bzImage to boot                    |
| `QEMU_BIN`      | `$(which qemu-system-x86_64)`  | QEMU system emulator                      |
| `BUSYBOX_BIN`   | `$(which busybox)`             | static busybox for the initramfs          |
| `DEMO_TIMEOUT`  | `120`                          | wall-clock guard, seconds                 |
| `KEEP_WORKDIR`  | `0`                            | keep the scratch initramfs dir if `1`     |

## What it demonstrates

* Hermit runs a **real hardware emulator (QEMU)** as its guest, and QEMU boots a
  **real Linux kernel** (the host's own `/boot/vmlinuz`, 6.17.x) on a busybox
  initramfs.
* The guest runs ordinary programs (`uname`, `cat /proc/version`, `id`, `date`,
  `/proc` probes). The guest clock reads Hermit's **deterministic virtual-time
  epoch** (2022-01-01), not host wall time.
* The initramfs `/init` **powers off** on its own, so the whole run terminates
  with exit 0 — nothing to babysit.

## Why the specific flags

Booting a multi-threaded VMM needs a particular relaxed profile (details inline
in `demo.sh` and in [`docs/QEMU_BOOT.md`](../../docs/QEMU_BOOT.md)):

* `--no-sequentialize-threads` — QEMU has a CPU-bound TCG vCPU thread plus
  main-loop/helper threads; serializing them onto one logical CPU starves the
  helpers and the boot makes ~no progress.
* `--preemption-timeout 10000000000` — a preemption slice larger than the whole
  boot, i.e. effectively "don't preempt the vCPU mid-boot" (meaningful
  preemption stalls it).
* QEMU `-icount shift=0,sleep=off` — one instruction-derived clock for the whole
  VM. Without it the guest sees two skewed clock domains (synthetic per-thread
  RDTSC vs the global-time PIT/APIC/PM timers) and Linux drops its clocksource.

## Assurance level

This is a **virtual-time compatibility boot** (backend: ptrace; relaxations:
`--no-sequentialize-threads`, high `--preemption-timeout`). It is **not** a
`--strict`/`--verify` (L2) determinism claim: with concurrency relaxed, QEMU's
host-thread interleavings are uncontrolled. A fully deterministic VM boot
(removing `--no-sequentialize-threads`) is the known next milestone — today
`--strict` stalls the boot because `--sequentialize-threads` starves QEMU's
helper threads.
