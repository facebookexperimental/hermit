#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# =============================================================================
#  demo.sh — Boot a real Linux kernel inside QEMU, under Hermit, to a clean
#            power-off, and show guest programs running with timestamps.
# =============================================================================
#
# WHAT THIS SHOWS
#   Hermit runs QEMU as its guest process. QEMU in turn boots an unmodified
#   Linux kernel (the host's own /boot/vmlinuz) on a busybox initramfs. The
#   initramfs `/init` runs a handful of ordinary guest programs (uname, id,
#   the guest clock, /proc probes) and then powers the machine off cleanly, so
#   the whole run terminates on its own with exit status 0 — no interactive
#   shell to babysit.
#
#   This is a *virtual-time compatibility* profile, NOT a --strict/--verify
#   determinism profile. See "WHY THESE FLAGS" below and the ASSURANCE note at
#   the end. Booting a multi-threaded VMM (QEMU) fully deterministically under
#   --strict is a known open milestone (--sequentialize-threads starves QEMU's
#   helper threads); this demo deliberately uses the relaxed profile that boots.
#
# USAGE
#   ./experiments/qemu-linux/demo.sh              # build initramfs + boot
#   HERMIT_BIN=target/debug/hermit ./...          # pick a hermit binary
#   KERNEL=/boot/vmlinuz-X ./...                  # pick a kernel bzImage
#   DEMO_TIMEOUT=120 ./...                        # wall-clock guard (seconds)
#   KEEP_WORKDIR=1 ./...                          # don't delete the scratch dir
#
# VERIFY
#   Exit 0 and the transcript contains HERMIT-QEMU-DEMO-BOOT-OK and
#   HERMIT-QEMU-DEMO-DONE.
# =============================================================================

set -uo pipefail

# --- Locate the repo root (this script lives in experiments/qemu-linux/) ------
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT" || exit 1

# --- Configuration (all overridable via the environment) ----------------------
QEMU_BIN="${QEMU_BIN:-$(command -v qemu-system-x86_64 || true)}"
BUSYBOX_BIN="${BUSYBOX_BIN:-$(command -v busybox || echo /usr/sbin/busybox)}"
KERNEL="${KERNEL:-/boot/vmlinuz}"
DEMO_TIMEOUT="${DEMO_TIMEOUT:-120}"
KEEP_WORKDIR="${KEEP_WORKDIR:-0}"

# Prefer a release hermit (fast); fall back to debug; then to a bare `hermit`.
pick_hermit() {
    if [[ -n "${HERMIT_BIN:-}" ]]; then echo "$HERMIT_BIN"; return; fi
    for c in target/release/hermit target/debug/hermit; do
        [[ -x "$c" ]] && { echo "$c"; return; }
    done
    command -v hermit || echo target/release/hermit
}
HERMIT_BIN="$(pick_hermit)"

# --- Small presentation helpers ----------------------------------------------
START_EPOCH=$(date +%s)
ts() {  # host wall-clock timestamp + elapsed since start, e.g. [15:04:07 +6s]
    local now; now=$(date +%s)
    printf '[%s +%ss]' "$(date +%H:%M:%S)" "$((now - START_EPOCH))"
}
say()  { printf '%s %s\n' "$(ts)" "$*"; }
rule() { printf '%s\n' "------------------------------------------------------------------"; }
head_rule() { printf '%s\n' "=================================================================="; }

# --- Preflight ----------------------------------------------------------------
head_rule
echo "  Hermit + QEMU + Linux — live boot demo"
head_rule
say "Preflight: checking tools and artifacts"

fail=0
[[ -x "$HERMIT_BIN" ]]  || { echo "  !! hermit binary not found/executable: $HERMIT_BIN (build with: cargo build --release -p hermit-cli)"; fail=1; }
[[ -n "$QEMU_BIN" && -x "$QEMU_BIN" ]] || { echo "  !! qemu-system-x86_64 not found"; fail=1; }
[[ -r "$KERNEL" ]]      || { echo "  !! kernel image not readable: $KERNEL"; fail=1; }
[[ -x "$BUSYBOX_BIN" ]] || { echo "  !! busybox not found: $BUSYBOX_BIN"; fail=1; }
[[ $fail -eq 0 ]] || { echo "Preflight failed — see messages above."; exit 2; }

# --- Component versions (for the presentation header) -------------------------
HERMIT_VER="$("$HERMIT_BIN" --version 2>/dev/null | head -1)"
HERMIT_GIT="$(git -C "$REPO_ROOT" describe --always --dirty 2>/dev/null || echo unknown)"
QEMU_VER="$("$QEMU_BIN" --version 2>/dev/null | head -1)"
KERNEL_REAL="$(readlink -f "$KERNEL")"
KERNEL_VER="$(basename "$KERNEL_REAL" | sed 's/^vmlinuz-//')"

rule
echo "  hermit   : ${HERMIT_VER:-?}   (bin: $HERMIT_BIN, git: $HERMIT_GIT)"
echo "  qemu     : ${QEMU_VER:-?}"
echo "  kernel   : $KERNEL_VER"
echo "             ($KERNEL_REAL)"
echo "  busybox  : $("$BUSYBOX_BIN" 2>&1 | head -1 | cut -c1-52)"
echo "  host     : $(uname -srm)"
rule

# --- Build a minimal, auto-poweroff initramfs --------------------------------
# Self-contained: we build the initramfs from the static busybox every run so
# the demo does not depend on any pre-staged (git-ignored) artifacts.
#
# IMPORTANT: the scratch dir must live OUTSIDE host /tmp. Hermit gives the guest
# (QEMU) an isolated /tmp, so an initramfs placed under the real /tmp is invisible
# to QEMU ("error reading initrd ... No such file"). We build under the repo's
# git-ignored target/ instead (passed through to the guest unchanged); this
# matches the paths the working QEMU-boot experiments used. (Alternatively one
# could add `--tmp=/tmp` to the hermit flags to expose the host /tmp.)
mkdir -p "$REPO_ROOT/target"
WORKDIR="$(mktemp -d "$REPO_ROOT/target/hermit-qemu-demo.XXXXXX")"
cleanup() { [[ "$KEEP_WORKDIR" == "1" ]] || rm -rf "$WORKDIR"; }
trap cleanup EXIT
ROOT="$WORKDIR/initramfs"
INITRD="$WORKDIR/initramfs.cpio.gz"

say "Building auto-poweroff initramfs in $WORKDIR"
mkdir -p "$ROOT"/{bin,proc,sys,dev}
cp "$BUSYBOX_BIN" "$ROOT/bin/busybox"
# Wire up the busybox applets the guest /init uses.
for app in sh cat mount umount uname poweroff id date grep head cut sed tr ls \
           echo sleep mknod dmesg hostname sync true; do
    ln -sf busybox "$ROOT/bin/$app"
done

# Guest init: mount pseudo-filesystems, print a boot marker, run a few ordinary
# guest programs, then power off cleanly. Every line is prefixed on the host
# side so the transcript reads like a narrated session.
cat > "$ROOT/init" <<'GUEST_INIT'
#!/bin/busybox sh
export PATH=/bin
mount -t proc     none /proc 2>/dev/null
mount -t sysfs    none /sys  2>/dev/null
mount -t devtmpfs none /dev  2>/dev/null || mount -t tmpfs none /dev 2>/dev/null
[ -c /dev/ttyS0 ] || mknod /dev/ttyS0 c 4 64 2>/dev/null

echo "=================================================================="
echo "HERMIT-QEMU-DEMO-BOOT-OK"
echo "  kernel  : $(uname -r)"
echo "  arch    : $(uname -m)"
echo "  hostname: $(hostname)"
echo "=================================================================="
echo "--- guest program 1/5:  uname -a"
uname -a
echo "--- guest program 2/5:  cat /proc/version"
cat /proc/version
echo "--- guest program 3/5:  id  (running as the guest's init/root)"
id
echo "--- guest program 4/5:  date -u  (guest clock == Hermit virtual time)"
date -u
echo "--- guest program 5/5:  /proc probes"
echo "  uptime : $(cat /proc/uptime)"
echo "  cpu    : $(grep -m1 -E 'model name|vendor_id' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"
echo "  mem    : $(grep -m1 MemTotal /proc/meminfo)"
echo "=================================================================="
echo "HERMIT-QEMU-DEMO-DONE"
echo "powering off..."
sync
poweroff -f
GUEST_INIT
chmod +x "$ROOT/init"

# Pack the newc cpio the kernel expects.
( cd "$ROOT" && find . -print0 \
    | cpio --null --create --format=newc 2>/dev/null \
    | gzip -9 > "$INITRD" )
say "initramfs built: $(du -h "$INITRD" | cut -f1) ($INITRD)"

# --- The boot command ---------------------------------------------------------
# Hermit flags (and WHY each is needed for a multi-threaded VMM guest):
#
#   run                          run a guest process under Detcore/Reverie.
#   --log error                  keep Hermit's own logging quiet for the demo.
#   --no-sequentialize-threads   REQUIRED. QEMU has a CPU-bound TCG vCPU thread
#                                plus main-loop/helper threads; serializing them
#                                onto one logical CPU starves the helpers and the
#                                guest kernel makes ~no progress. (This is also
#                                exactly why --strict does NOT boot QEMU today.)
#   --max-timeslice 10^10   set the PMU-RCB preemption slice larger than the
#                                whole boot, i.e. effectively "don't preempt the
#                                vCPU thread mid-boot" (meaningful preemption
#                                stalls the boot).
#
# QEMU flags (and WHY):
#   -accel tcg,thread=single     software emulation, single TCG thread (no KVM,
#                                so instruction execution is interposable).
#   -icount shift=0,sleep=off    REQUIRED. One instruction-derived clock for the
#                                whole VM. Without it the guest sees two skewed
#                                clock domains (synthetic per-thread RDTSC vs the
#                                global-time PIT/APIC/PM timers) and Linux drops
#                                its clocksource.
#   -serial stdio                guest console on our stdout (what you see below).
#   -no-reboot                   turn the guest's power-off into a QEMU exit(0).
#   -append '...rdinit=/init'    boot straight into our busybox /init.
HERMIT_FLAGS=( --log error run --no-sequentialize-threads --max-timeslice 10000000000 )
QEMU_FLAGS=( -m 256M -accel tcg,thread=single -smp 1 -icount shift=0,sleep=off
             -kernel "$KERNEL_REAL" -initrd "$INITRD"
             -display none -serial stdio -monitor none -no-reboot
             -append "console=ttyS0 panic=-1 rdinit=/init" )

rule
echo "  Command:"
echo "    $HERMIT_BIN ${HERMIT_FLAGS[*]} -- \\"
echo "      $QEMU_BIN ${QEMU_FLAGS[*]}"
rule
say "Booting Linux under Hermit+QEMU (wall-clock guard: ${DEMO_TIMEOUT}s)"
echo "..................................................................."

BOOT_START=$(date +%s)
# --kill-after so a stuck run cannot leave an orphaned hermit/qemu behind.
timeout --kill-after=10 --signal=TERM "$DEMO_TIMEOUT" \
    "$HERMIT_BIN" "${HERMIT_FLAGS[@]}" -- "$QEMU_BIN" "${QEMU_FLAGS[@]}"
RC=$?
BOOT_END=$(date +%s)
echo "..................................................................."

# --- Verdict ------------------------------------------------------------------
rule
BOOT_SECS=$((BOOT_END - BOOT_START))
say "QEMU/Hermit process exited: rc=$RC   (boot-to-poweroff wall time: ${BOOT_SECS}s)"
if [[ $RC -eq 0 ]]; then
    echo "  RESULT: PASS — the guest booted Linux, ran its programs, and powered"
    echo "          itself off cleanly (exit 0)."
elif [[ $RC -eq 124 || $RC -eq 137 ]]; then
    echo "  RESULT: TIMEOUT — the run hit the ${DEMO_TIMEOUT}s wall-clock guard."
    echo "          If the boot marker appeared above, the boot itself worked and"
    echo "          only the power-off/exit was slow; otherwise the boot stalled."
else
    echo "  RESULT: exit $RC — see the transcript above."
fi
rule
echo "  ASSURANCE: virtual-time COMPATIBILITY boot (backend: ptrace; relaxations:"
echo "  --no-sequentialize-threads, high --max-timeslice). This is NOT a"
echo "  --strict/--verify (L2) determinism claim: with concurrency relaxed, QEMU's"
echo "  host-thread interleavings are uncontrolled. Fully deterministic VM boot"
echo "  (removing --no-sequentialize-threads) is the known next milestone."
head_rule

exit "$RC"
