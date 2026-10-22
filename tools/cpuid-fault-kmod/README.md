# CPUID faulting for KVM guests on Linux 6.16 and older

Hermit virtualizes the `cpuid` instruction by asking Linux to make it fault,
with `arch_prctl(ARCH_SET_CPUID, 0)`. Without that, the default ptrace
backend stops with `ptrace-cpuid-faulting-unavailable ... errno=19`,
`--cpu-profile=host-no-rdrand` is refused with `CPUID faulting is unavailable`,
and the LiteInst and in-guest-trap backends fail to initialize. The only way
around it is `--no-virtualize-cpuid`, which lets host CPUID into the guest.

`enable-cpuid-faulting.sh` turns CPUID faulting on, until the next reboot, on
hosts where the hypervisor supports it but the kernel does not detect it.

## License

Everything in this directory is licensed **GPL-2.0-only**, as each file's SPDX
header says. This is separate from the rest of the repository, which is under
the BSD 3-Clause license in the top-level [LICENSE](../../LICENSE). The kernel
module is built against Linux's GPL-2.0 headers and declares
`MODULE_LICENSE("GPL")`. The probe, Makefile and script use the same license so
that the whole directory has one license. License text:
[GPL-2.0-only](https://spdx.org/licenses/GPL-2.0-only.html).

## When you need it

You need it on a KVM guest that meets all of these:

- it presents an AMD vCPU model (for example "AMD EPYC-Genoa");
- it runs Linux 6.16 or older;
- its hypervisor implements Intel-style CPUID faulting
  (`MSR_PLATFORM_INFO` bit 31, toggled through `MSR_MISC_FEATURES_ENABLES`).

On-demand cloud development VMs (AMD Genoa KVM guests on Linux 6.16) are like
this. Linux before 6.17 probes that interface only for Intel CPUs, so on these
guests `X86_FEATURE_CPUID_FAULT` stays clear and `ARCH_SET_CPUID` returns
`ENODEV`, even though the hypervisor supports it.

You do not need it, and the script changes nothing, where faulting already
works. That includes bare-metal Intel hosts, and AMD hosts on Linux 6.17 or
later whose CPU supports it. Check with:

```bash
tools/cpuid-fault-kmod/enable-cpuid-faulting.sh --status
```

## Use it before reproducible builds

Run this once per boot, from a Hermit checkout, before running builds under
Hermit:

```bash
tools/cpuid-fault-kmod/enable-cpuid-faulting.sh --hermit "$(command -v hermit)"
```

Without `--hermit`, the script stops after its own probe. Each privileged step
runs through `sudo` and is printed before it runs. In order, the script:

1. Builds a small probe (`cpuid-fault-probe.c`) and runs it. If
   `ARCH_SET_CPUID(0)` works and `cpuid` traps, it reports "already
   available" and exits 0 without changing anything.
2. Refuses unless all of these hold: the machine is x86_64, the kernel is older
   than 6.17, and `/proc/cpuinfo` has the `hypervisor` flag.
3. Reads `MSR_PLATFORM_INFO` (0xce) and `MSR_MISC_FEATURES_ENABLES` (0x140)
   through `/dev/cpu/*/msr` on every online CPU. It refuses unless every CPU
   has bit 31 set and 0x140 equal to 0.
4. Installs `kernel-devel-$(uname -r)` with dnf if the build tree is missing.
   The on-demand VMs' kernels are built by clang with ThinLTO, and gcc rejects
   their flags (`-fsplit-lto-unit`). For such a kernel, the script installs the
   `clang`, `lld` and `llvm` packages whose version matches
   `CONFIG_CLANG_VERSION`, and the Makefile builds with `LLVM=1`.
   Installing that exact version can make dnf downgrade packages that share
   LLVM's libraries. On the test VM it downgraded `llvm-libs` and the Mesa
   packages to match 21.1.7. `--no-install` refuses instead of installing.
5. Builds `hermit_cpuid_fault.ko` in
   `${XDG_CACHE_HOME:-$HOME/.cache}/hermit-cpuid-fault-kmod/$(uname -r)` and
   checks that its vermagic matches the running kernel.
6. Loads it with `insmod`. On failure it shows the module's kernel messages,
   which name the CPU and check that failed.
7. Runs the probe again and requires a trap. With `--hermit PATH`, it then also
   runs `PATH run --verify -- /bin/true`.

Every refusal names the check that failed and says what to do next. Nothing
falls back silently. Exit status: 0 for success or already available, 1 for a
refusal or failure, 2 for a usage error.

To undo it:

```bash
tools/cpuid-fault-kmod/enable-cpuid-faulting.sh --disable
```

This writes 1 to `/sys/module/hermit_cpuid_fault/parameters/release`, runs
`rmmod`, and checks that `ARCH_SET_CPUID` fails again.

## What the module does, and why it is safe

- **It writes no MSR.** It only sets the kernel's `X86_FEATURE_CPUID_FAULT`
  capability bit. Linux 6.16's own `set_cpuid_faulting()` is already
  vendor-neutral, and from then on it sets bit 0 of 0x140 only on a CPU while
  that CPU runs a task that asked for faulting. Every other task, and the host,
  executes `cpuid` natively.
- **It checks every CPU before it publishes anything.** A CPU-hotplug online
  callback runs on each online CPU. It requires `MSR_PLATFORM_INFO` bit 31 and
  `MSR_MISC_FEATURES_ENABLES == 0`, which makes the kernel's unexported per-CPU
  shadow of that MSR (still its static 0) equal to the hardware. Only after
  every CPU passes does the module set the bit that `arch_prctl` reads. A CPU
  hot-added later that fails the check is kept offline.
- **It refuses to load where it does not apply.** Loading fails with `EEXIST`
  if the kernel already has the capability, and with `ENODEV` without a
  hypervisor. The source refuses to build for Linux 6.17 and later, whose
  `set_cpuid_faulting()` uses AMD's `MSR_K7_HWCR` instead.
- **Unloading is guarded.** The module holds a reference to itself, so a plain
  `rmmod` fails until a release succeeds. The release clears the capability,
  waits an RCU grace period, and then scans every thread. If any thread still
  has CPUID faulting armed (`TIF_NOCPUID`, for example a running Hermit guest),
  it restores the capability, fails with `EBUSY`, and names the first such pid
  in `dmesg`.
- Loading an out-of-tree module taints the kernel (the `O` flag).

The header comment in `hermit_cpuid_fault.c` gives the full kernel-source
argument.

## What was tested, and what was not

Tested on freshly reserved on-demand cloud VMs (AMD EPYC-Genoa KVM guests,
166 vCPUs, Linux 6.16.1 built by clang 21.1.7):

- the probe traps on all 166 CPUs, and only for the armed task;
- `hermit run --verify` passes with CPUID virtualized under default ptrace,
  `--cpu-profile=host-no-rdrand`, LiteInst and in-guest-trap;
- release and `rmmod` return the host to its exact earlier state;
- release is refused while an armed process is alive.

Not tested: CPU hot-add, suspend and resume, and live migration of the VM.
After a migration, the MSR state depends on KVM restoring
`MSR_MISC_FEATURES_ENABLES`.
