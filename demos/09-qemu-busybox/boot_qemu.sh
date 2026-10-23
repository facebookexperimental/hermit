#!/usr/bin/env bash
# Boot the demo kernel and BusyBox initramfs in single-threaded QEMU TCG with
# instruction counting, so guest time advances with executed instructions.

set -euo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

kernel_image=${1:-${KERNEL_IMAGE:-}}
initramfs_image=${2:-${INITRAMFS_IMAGE:-}}
qemu_bin=${3:-${QEMU_BIN:-}}

# Work out the checkout only when the default initramfs path is needed. This
# script runs inside Hermit, and bash's `cd` examines every directory on the way
# to the checkout. Those directories' sizes change whenever a file is created
# or removed in them, for example in your home directory, and Hermit reports
# them as they are. A VERIFY=1 run failed its comparison on that alone, so
# run.sh passes every path as an argument and this block is skipped.
if [[ -z $initramfs_image ]]; then
  script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
  repo_root=$(cd -- "$script_dir/../.." && pwd)
  initramfs_image=$repo_root/target/qemu-busybox/initramfs-busybox.cpio.gz
fi

if [[ -z $qemu_bin ]]; then
  qemu_bin=$(command -v qemu-system-x86_64 || true)
fi

[[ -n $kernel_image ]] || fail "set KERNEL_IMAGE or pass a kernel image as argument 1"
[[ -r $kernel_image ]] || fail "kernel image is not readable: $kernel_image"
[[ -r $initramfs_image ]] || fail \
  "initramfs is not readable: $initramfs_image (run demos/09-qemu-busybox/build-initramfs.sh)"
[[ -n $qemu_bin && -x $qemu_bin ]] || fail \
  "qemu-system-x86_64 not found; install it, set QEMU_BIN, or pass it as argument 3"

exec "$qemu_bin" \
  -nodefaults \
  -nic none \
  -machine q35 \
  -cpu max \
  -m 256M \
  -accel 'tcg,thread=single' \
  -smp 1 \
  -icount 'shift=0,sleep=on' \
  -rtc 'base=utc,clock=vm' \
  -kernel "$kernel_image" \
  -initrd "$initramfs_image" \
  -display none \
  -serial stdio \
  -monitor none \
  -no-reboot \
  -append 'console=ttyS0 panic=-1 rdinit=/init'
