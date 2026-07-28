# Hermit demos

## BusyBox inside QEMU/Linux

`05-qemu-busybox.sh` builds a reproducible initramfs around a statically linked
BusyBox, boots it as QEMU's Linux userspace under Hermit, and requires the
guest workload to finish successfully. The workload runs `uname`, filesystem
traversal, a shell pipeline, arbitrary-precision `bc`, and SHA-256 before
powering down.

Build Hermit and run one strict boot:

```bash
cargo build --release -p hermit --bin hermit
KERNEL_IMAGE=/path/to/bzImage ./demos/05-qemu-busybox.sh
```

The default backend is ptrace. This command uses `--log info --strict` with no
determinism relaxations, so a successful run is L1 evidence for the exact
kernel, BusyBox, QEMU, and host recorded in the output. Logs and generated
artifacts are written under `target/qemu-busybox/`. The Linux serial console is
streamed live and saved as `console.log`; Hermit's full INFO trace is saved as
`hermit-info.log` without flooding the serial output.

To exercise the QEMU launcher directly under `cargo run`, build the initramfs
once on the host and supply both fixed images:

```bash
BUSYBOX=/path/to/static/busybox \
  ./demos/qemu-busybox/build-initramfs.sh
KERNEL_IMAGE=/path/to/bzImage \
  INITRAMFS_IMAGE=target/qemu-busybox/initramfs-busybox.cpio.gz \
  cargo run --release -p hermit -- run --strict -- demos/boot_qemu.sh
```

`boot_qemu.sh` only validates its inputs and then replaces itself with QEMU.
The higher-level `05-qemu-busybox.sh` uses the same launcher while adding asset
construction, timeouts, live serial output, log capture, and result checks.

Run Hermit's two-execution comparison for L2 evidence:

```bash
KERNEL_IMAGE=/path/to/bzImage VERIFY=1 \
  DEMO_TIMEOUT_SECONDS=900 ./demos/05-qemu-busybox.sh
```

`VERIFY=1` adds `--verify` and requires Hermit's explicit
`Determinism verified` marker. It can take several minutes because QEMU's host
threads execute under the strict ptrace scheduler. Hermit captures guest output
internally in this mode so it can compare both executions; the verification
summary is saved as `hermit-stderr.log` instead of replaying the serial console.

The ptrace PMU skid margin is processor-specific. If Reverie reports that its
perf counter exceeded the single-step target, measure the host and pass the
reported recommendation explicitly:

```bash
cc -O2 -Wall -Wextra -Werror -std=gnu11 \
  tests/util/pmu_skid.c -o /tmp/pmu-skid-test
/tmp/pmu-skid-test --iterations 1000
KERNEL_IMAGE=/path/to/bzImage VERIFY=1 SKID_MARGIN=66276 \
  DEMO_TIMEOUT_SECONDS=1000 ./demos/05-qemu-busybox.sh
```

`SKID_MARGIN` forwards to Hermit's `--skid-margin`; it schedules the PMU
overflow earlier and can add single-stepping overhead, but does not disable a
determinism feature. Use the value measured on the host rather than copying the
example blindly.

### Inputs and dependencies

- x86-64 Linux with `qemu-system-x86_64`
- a readable x86-64 `bzImage` supplied with `KERNEL_IMAGE`
- a statically linked BusyBox supplied with `BUSYBOX` when it is not on `PATH`
- `cpio`, `file`, `find`, `gzip`, `install`, `sha256sum`, `sort`, `stat`,
  `tee`, `timeout`, `touch`, and `wc`

The runner pins QEMU to the `q35` machine, `max` CPU, one TCG vCPU, and an
instruction-derived clock. It disables default devices and networking and uses
a VM-clock RTC. The initramfs builder fixes archive order, ownership, mtimes,
cpio inode metadata, and the gzip header. A changing kernel, QEMU binary,
BusyBox binary, host filesystem, or command line is a different experiment.

### Current boundary

This demo proves deterministic execution of a fixed, noninteractive initramfs
workload. It does not provide a writable persistent root disk, networking,
snapshot/resume, or Linux record/replay. QEMU record/replay remains downstream
of making its host threads compatible with Hermit's sequentialized recording
scheduler.

The ptrace backend must be permitted to trace its own child processes. A
container seccomp policy or host Yama/LSM policy that denies
`PTRACE_TRACEME` blocks the run before QEMU starts. The demo deliberately
requires an external kernel image rather than silently choosing a host- or
network-dependent kernel.

### Measured result

One L1 run on 2026-07-27 completed on the ptrace backend with `--log info
--strict` and no relaxations. It used QEMU 10.1.0, Linux 6.17.13, kernel SHA-256
`e4b1c0248a31c7e1f7cb31d82a1a03d4e7cab408ee1b8e622dd897c17eae46a2`,
BusyBox SHA-256
`e35db14651077c08598fbc3259609b2db398e5b7dcf07b28f1f3156118bcc081`,
and initramfs SHA-256
`5515b4bced678c4d22ff54dafd1676f06b8e254f1656d2994018df24aa1e9698`.
The guest reached `HERMIT-QEMU-BUSYBOX-PASS`, printed pi as `3.1415926532`,
and powered down. With `boot_qemu.sh` as the guest entry point, Hermit scheduled
six shell/QEMU threads for 38,088 turns over 181.740147850 virtual seconds. The
documented `cargo run` form and the higher-level runner both produced console
SHA-256 `f9a42014fac177223f08d5e722a8c6d88ae3b79eb0f1fab95bbdcb15487fbab3`.
Pinning `q35` and `max` produced no QEMU stderr warnings.

On the measured AMD EPYC 9D85 host, the repository PMU benchmark observed a
33,138-RCB maximum skid over 1,000 samples and recommended a 66,276-RCB margin.
Current Reverie's 1,000-RCB processor default panicked during L2 Run 1. The
measured override passed the prior failure point, but Run 1 had not completed
after nine minutes and was stopped; current-main L2 verification therefore
remains blocked on practical PMU calibration.

Before the Reverie PMU-default update, an L2 `VERIFY=1` run completed both q35
boots with the same inputs. Each normalized log contained 759,956 messages,
including 548,255 DETLOG and scheduler COMMIT messages. Hermit reported no
substantive differences and printed `Success: deterministic. Determinism
verified.`
