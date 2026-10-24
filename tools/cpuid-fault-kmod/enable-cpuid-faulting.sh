#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
#
# Make Linux CPUID faulting (arch_prctl ARCH_SET_CPUID), which Hermit needs to
# virtualize CPUID, available on a KVM guest whose hypervisor implements it but
# whose Linux 6.16-or-older kernel does not detect it (AMD vCPU models). It
# builds and loads hermit_cpuid_fault.ko from this directory. See README.md.
set -euo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly HERE
readonly MODULE=hermit_cpuid_fault
readonly RELEASE_PARAM=/sys/module/$MODULE/parameters/release
readonly SOURCES=(hermit_cpuid_fault.c cpuid-fault-probe.c Makefile)

usage() {
    cat <<EOF
Usage: $(basename -- "$0") [--status | --disable] [OPTIONS]

Make Linux CPUID faulting available for Hermit on a KVM guest that runs
Linux 6.16 or older on an AMD vCPU model (for example on-demand cloud VMs),
by building and loading the hermit_cpuid_fault kernel module from this
directory. The module only sets the kernel's X86_FEATURE_CPUID_FAULT
capability after checking every CPU; it never writes an MSR itself.

Modes:
  (default)      If arch_prctl(ARCH_SET_CPUID) already works, say so and
                 change nothing. Otherwise check the host (hypervisor present,
                 MSR_PLATFORM_INFO bit 31 on every CPU, Linux older than 6.17),
                 install kernel-devel (and, for a clang-built kernel, the
                 matching clang, lld and llvm) with dnf, build the module,
                 load it, and verify faulting with a probe.
  --status       Report whether CPUID faulting works and whether the module is
                 loaded. Changes nothing and needs no sudo. Exit 0 when
                 faulting works, 1 when it does not.
  --disable      Release and unload the module (refused while any process has
                 CPUID faulting armed), then confirm faulting is gone.

Options:
  --hermit PATH  After faulting works, also run
                 'PATH run --verify -- /bin/true' as an end-to-end check.
  --build-dir D  Build directory (default:
                 \${XDG_CACHE_HOME:-\$HOME/.cache}/hermit-cpuid-fault-kmod/<uname -r>).
  --no-install   Refuse instead of installing missing packages with dnf.
  -h, --help     Show this help.

Privileged steps run through sudo (unless already root) and are printed
before they run. The module does not survive a reboot; rerun this script.

Exit status: 0 success (or already available), 1 refused or failed (the
message names the failed check), 2 usage error.
EOF
}

say() { printf '[cpuid-fault] %s\n' "$*"; }
refuse() {
    printf '[cpuid-fault] REFUSED: %s\n' "$*" >&2
    exit 1
}
run() {
    printf '[cpuid-fault] + %s\n' "$*"
    "$@"
}

SUDO=()
if [[ $(id -u) -ne 0 ]]; then
    SUDO=(sudo)
fi
as_root() { run "${SUDO[@]}" "$@"; }

MODE=enable
HERMIT=
BUILD_DIR=
INSTALL=1
while [[ $# -gt 0 ]]; do
    case "$1" in
        --status) MODE=status ;;
        --disable) MODE=disable ;;
        --hermit)
            [[ $# -ge 2 ]] || { echo "--hermit needs a path; see --help" >&2; exit 2; }
            HERMIT=$2
            shift
            ;;
        --hermit=*) HERMIT=${1#--hermit=} ;;
        --build-dir)
            [[ $# -ge 2 ]] || { echo "--build-dir needs a directory; see --help" >&2; exit 2; }
            BUILD_DIR=$2
            shift
            ;;
        --build-dir=*) BUILD_DIR=${1#--build-dir=} ;;
        --no-install) INSTALL=0 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1; see --help" >&2
            exit 2
            ;;
    esac
    shift
done

KERNEL_RELEASE=$(uname -r)
readonly KERNEL_RELEASE
readonly KDIR=/lib/modules/$KERNEL_RELEASE/build
[[ -n $BUILD_DIR ]] || BUILD_DIR=${XDG_CACHE_HOME:-$HOME/.cache}/hermit-cpuid-fault-kmod/$KERNEL_RELEASE

if [[ -n $HERMIT && ! -x $HERMIT ]]; then
    refuse "--hermit $HERMIT is not an executable file; pass the path of a hermit binary"
fi

module_loaded() { [[ -d /sys/module/$MODULE ]]; }

# Build the probe into a private temporary directory. It is tiny and built
# fresh each time, so it always matches the source next to this script.
PROBE_DIR=$(mktemp -d "${TMPDIR:-/tmp}/cpuid-fault-probe.XXXXXX")
trap 'rm -f -- "$PROBE_DIR/cpuid-fault-probe" && rmdir -- "$PROBE_DIR"' EXIT
PROBE=$PROBE_DIR/cpuid-fault-probe
build_probe() {
    command -v cc >/dev/null 2>&1 ||
        refuse "no C compiler: 'cc' is not on PATH, so the probe cannot be built; install gcc (sudo dnf -y install gcc) and rerun"
    run cc -O2 -Wall -Wextra -Werror -Wl,-z,now -o "$PROBE" "$HERE/cpuid-fault-probe.c" ||
        refuse "building cpuid-fault-probe.c failed (output above); fix the C toolchain and rerun"
}

# Sets PROBE_RC and PROBE_OUT. 0 works, 1 unavailable, 2 broken, 3 query failed.
probe() {
    PROBE_RC=0
    PROBE_OUT=$("$PROBE" 2>&1) || PROBE_RC=$?
    say "probe: $PROBE_OUT (exit $PROBE_RC)"
}

refuse_broken_probe() {
    refuse "the kernel accepted ARCH_SET_CPUID(0) but CPUID did not fault ($PROBE_OUT). Hermit's CPUID virtualization is not safe on this host; do not use it here.${1:-}"
}

# Remember how long the kernel log is, so module_dmesg shows only what the
# module logged after this point.
DMESG_MARK=0
mark_dmesg() {
    DMESG_MARK=$("${SUDO[@]}" dmesg | wc -l) || DMESG_MARK=0
}
module_dmesg() {
    say "kernel messages from $MODULE since this step began (sudo dmesg):"
    "${SUDO[@]}" dmesg | tail -n +$((DMESG_MARK + 1)) | grep "$MODULE:" | sed 's/^/    /' || true
}

verify_with_hermit() {
    [[ -n $HERMIT ]] || return 0
    say "end-to-end check: Hermit with CPUID virtualized (the default)"
    if ! run "$HERMIT" run --verify -- /bin/true; then
        refuse "'$HERMIT run --verify -- /bin/true' failed (output above) although the probe shows CPUID faulting works; that failure is not a CPUID-faulting problem"
    fi
    say "Hermit run --verify passed"
}

kernel_older_than_6_17() {
    local major minor
    IFS=.- read -r major minor _ <<<"$KERNEL_RELEASE"
    [[ $major =~ ^[0-9]+$ && $minor =~ ^[0-9]+$ ]] ||
        refuse "cannot parse the kernel version from uname -r ($KERNEL_RELEASE)"
    ((major < 6 || (major == 6 && minor < 17)))
}

status_mode() {
    say "kernel: $KERNEL_RELEASE"
    if grep -qw hypervisor /proc/cpuinfo; then
        say "hypervisor: yes (/proc/cpuinfo has the hypervisor flag)"
    else
        say "hypervisor: no (/proc/cpuinfo lacks the hypervisor flag)"
    fi
    say "CPUs whose /proc/cpuinfo lists cpuid_fault: $(grep -c '^flags.*\bcpuid_fault\b' /proc/cpuinfo || true) of $(grep -c '^processor' /proc/cpuinfo)"
    if module_loaded; then
        say "module: $MODULE is loaded"
    else
        say "module: $MODULE is not loaded"
    fi
    build_probe
    probe
    case $PROBE_RC in
        0)
            say "CPUID faulting: available"
            exit 0
            ;;
        1)
            say "CPUID faulting: unavailable; run $(basename -- "$0") without --status to enable it"
            exit 1
            ;;
        *) refuse_broken_probe ;;
    esac
}

disable_mode() {
    build_probe
    if ! module_loaded; then
        say "$MODULE is not loaded; nothing to disable"
        probe
        exit 0
    fi
    say "step 1/3: release the module (clears the capability; refused while any thread has CPUID faulting armed)"
    mark_dmesg
    printf '[cpuid-fault] + echo 1 | %s tee %s\n' "${SUDO[*]}" "$RELEASE_PARAM"
    if ! echo 1 | "${SUDO[@]}" tee "$RELEASE_PARAM" >/dev/null; then
        module_dmesg
        refuse "the module refused release: a process still has CPUID faulting armed (the kernel message above names the first one; usually a running Hermit guest). Stop it and rerun --disable"
    fi
    say "step 2/3: unload the module"
    as_root rmmod "$MODULE" || refuse "rmmod $MODULE failed (output above); check 'lsmod | grep $MODULE' and rerun --disable"
    module_dmesg
    say "step 3/3: confirm CPUID faulting is gone"
    probe
    case $PROBE_RC in
        1) say "done: $MODULE is unloaded and ARCH_SET_CPUID fails again, as before it was loaded" ;;
        0) say "done: $MODULE is unloaded; CPUID faulting still works, so this kernel provides it by itself" ;;
        *) refuse_broken_probe ;;
    esac
    exit 0
}

# Reads MSR_PLATFORM_INFO (0xce) and MSR_MISC_FEATURES_ENABLES (0x140) on every
# online CPU with one privileged shell, and checks bit 31 and zero.
check_msrs() {
    if [[ ! -e /dev/cpu/0/msr ]]; then
        say "/dev/cpu/0/msr is missing; loading the kernel's msr driver to read MSRs"
        as_root modprobe msr || refuse "modprobe msr failed (output above); the MSR checks need /dev/cpu/*/msr"
    fi
    say "reading MSR_PLATFORM_INFO (0xce) and MSR_MISC_FEATURES_ENABLES (0x140) on every online CPU through /dev/cpu/*/msr (sudo)"
    local lines
    # shellcheck disable=SC2016 # expanded by the privileged shell
    lines=$("${SUDO[@]}" bash -c '
        rd() { dd if="$1" bs=8 count=1 skip=$(($2)) iflag=skip_bytes status=none 2>/dev/null | od -An -tx8 | tr -d " \n"; }
        for d in /dev/cpu/[0-9]*/msr; do
            c=${d#/dev/cpu/}; c=${c%/msr}
            echo "$c $(rd "$d" 0xce) $(rd "$d" 0x140)"
        done') || refuse "reading /dev/cpu/*/msr failed; the MSR checks need sudo"
    local online total=0 good=0 bad=() cpu pinfo misc
    online=$(getconf _NPROCESSORS_ONLN)
    while read -r cpu pinfo misc; do
        total=$((total + 1))
        if [[ -z $pinfo ]]; then
            bad+=("cpu$cpu: MSR_PLATFORM_INFO is unreadable")
        elif (((16#$pinfo >> 31 & 1) == 0)); then
            bad+=("cpu$cpu: MSR_PLATFORM_INFO=0x$pinfo lacks bit 31 (CPUID faulting supported)")
        elif [[ -z $misc ]]; then
            bad+=("cpu$cpu: MSR_MISC_FEATURES_ENABLES is unreadable")
        elif ((16#$misc != 0)); then
            bad+=("cpu$cpu: MSR_MISC_FEATURES_ENABLES=0x$misc, expected 0")
        else
            good=$((good + 1))
        fi
    done <<<"$lines"
    if ((${#bad[@]} > 0)); then
        printf '[cpuid-fault]     %s\n' "${bad[@]:0:5}" >&2
        refuse "${#bad[@]} of $total CPUs fail the MSR check (first ones above): the hypervisor does not offer Intel-style CPUID faulting on every vCPU, so the module cannot help. Use a host that offers it, or run Hermit with --no-virtualize-cpuid (the guest then sees the host CPUID)"
    fi
    if ((total != online)); then
        refuse "read MSRs on $total CPUs but $online are online; rerun when CPU hotplug is quiet"
    fi
    say "MSR_PLATFORM_INFO bit 31 is set and MSR_MISC_FEATURES_ENABLES is 0 on all $good online CPUs"
}

dnf_install() {
    if ((INSTALL == 0)); then
        refuse "missing packages: $*; install them (sudo dnf -y install $*) or rerun without --no-install"
    fi
    as_root dnf -y install "$@" || refuse "dnf could not install $* (output above); install them by hand and rerun"
}

prepare_toolchain() {
    if [[ ! -e $KDIR/Makefile ]]; then
        say "kernel build tree $KDIR is missing; installing kernel-devel for the running kernel"
        dnf_install "kernel-devel-$KERNEL_RELEASE"
        [[ -e $KDIR/Makefile ]] ||
            refuse "kernel-devel-$KERNEL_RELEASE was installed but $KDIR/Makefile is still missing; check the package and the /lib/modules/$KERNEL_RELEASE/build link"
    fi
    say "kernel build tree: $KDIR"
    command -v make >/dev/null 2>&1 || dnf_install make
    if ! grep -qs '^CONFIG_CC_IS_CLANG=y' "$KDIR/.config"; then
        say "the kernel was built by gcc; building the module with gcc"
        return
    fi
    local code want have tool missing=()
    code=$(sed -n 's/^CONFIG_CLANG_VERSION=//p' "$KDIR/.config")
    [[ $code =~ ^[0-9]+$ ]] || refuse "$KDIR/.config says the kernel was built by clang but has no numeric CONFIG_CLANG_VERSION"
    want=$((code / 10000)).$((code / 100 % 100)).$((code % 100))
    say "the kernel was built by clang $want; the module must be built with LLVM=1 and the same clang"
    have=$(clang --version 2>/dev/null | sed -n '1s/.*clang version \([0-9][0-9.]*\).*/\1/p' || true)
    for tool in ld.lld llvm-ar llvm-nm llvm-objcopy; do
        command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
    done
    if [[ $have != "$want" || ${#missing[@]} -gt 0 ]]; then
        say "installed clang: ${have:-none}; missing LLVM tools: ${missing[*]:-none}; installing clang, lld and llvm $want"
        dnf_install "clang-$want" "lld-$want" "llvm-$want"
        have=$(clang --version 2>/dev/null | sed -n '1s/.*clang version \([0-9][0-9.]*\).*/\1/p' || true)
        [[ $have == "$want" ]] ||
            refuse "clang ${have:-none} is installed after dnf, but the kernel was built by clang $want; install clang-$want lld-$want llvm-$want by hand"
    fi
    say "clang $have matches the kernel's compiler"
}

build_module() {
    say "building in $BUILD_DIR"
    run mkdir -p -- "$BUILD_DIR"
    local f
    for f in "${SOURCES[@]}"; do
        cp -f -- "$HERE/$f" "$BUILD_DIR/$f"
    done
    run make -C "$BUILD_DIR" module || refuse "the module build failed (output above)"
    local vermagic
    vermagic=$(modinfo -F vermagic "$BUILD_DIR/$MODULE.ko")
    [[ $vermagic == "$KERNEL_RELEASE "* ]] ||
        refuse "built module's vermagic '$vermagic' does not match the running kernel $KERNEL_RELEASE"
    say "built $BUILD_DIR/$MODULE.ko (vermagic: $vermagic)"
}

enable_mode() {
    say "step 1/7: does CPUID faulting already work? (build and run cpuid-fault-probe)"
    build_probe
    probe
    case $PROBE_RC in
        0)
            if module_loaded; then
                say "CPUID faulting is already available ($MODULE is loaded); nothing changed"
            else
                say "CPUID faulting is already available (provided by the kernel); nothing changed"
            fi
            verify_with_hermit
            exit 0
            ;;
        1) ;;
        *) refuse_broken_probe ;;
    esac
    if module_loaded; then
        refuse "$MODULE is loaded but CPUID faulting does not work; run $(basename -- "$0") --disable, then rerun"
    fi

    say "step 2/7: check the kernel and CPU"
    [[ $(uname -m) == x86_64 ]] || refuse "this is $(uname -m); CPUID faulting is an x86_64 feature"
    if ! kernel_older_than_6_17; then
        refuse "Linux $KERNEL_RELEASE is 6.17 or later, where set_cpuid_faulting() uses AMD's MSR_K7_HWCR instead of MSR_MISC_FEATURES_ENABLES, so this module does not apply (it refuses to build there). Faulting is missing because the CPU or hypervisor does not offer it; use a host that does, or run Hermit with --no-virtualize-cpuid (the guest then sees the host CPUID)"
    fi
    say "Linux $KERNEL_RELEASE is older than 6.17"
    grep -qw hypervisor /proc/cpuinfo ||
        refuse "this host is not a virtual machine (no hypervisor flag in /proc/cpuinfo). The module only covers hypervisors that emulate Intel-style CPUID faulting; on bare metal use an Intel CPU, or an AMD CPU with Linux 6.17 or later"
    local virt
    virt=$(systemd-detect-virt --vm 2>/dev/null) || virt="hypervisor flag set"
    say "running under a hypervisor ($virt)"

    say "step 3/7: check every CPU's MSRs"
    check_msrs

    say "step 4/7: prepare the kernel module toolchain"
    prepare_toolchain

    say "step 5/7: build the module"
    build_module

    say "step 6/7: load the module"
    mark_dmesg
    if ! as_root insmod "$BUILD_DIR/$MODULE.ko"; then
        module_dmesg
        refuse "insmod failed (kernel messages above name the failed check)"
    fi
    module_dmesg

    say "step 7/7: verify CPUID faulting"
    probe
    case $PROBE_RC in
        0) ;;
        1) refuse "$MODULE is loaded but ARCH_SET_CPUID still fails; run $(basename -- "$0") --disable and report this" ;;
        *) refuse_broken_probe " Unload the module with $(basename -- "$0") --disable." ;;
    esac
    verify_with_hermit
    say "done: CPUID faulting is available until reboot or '$(basename -- "$0") --disable'"
}

case $MODE in
    status) status_mode ;;
    disable) disable_mode ;;
    enable) enable_mode ;;
esac
