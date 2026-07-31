#!/usr/bin/env bash

set -euo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
kernel_image=${1:-${KERNEL_IMAGE:-}}
initramfs_image=${2:-${INITRAMFS_IMAGE:-$repo_root/target/qemu-busybox/initramfs-busybox.cpio.gz}}
qemu_bin=${3:-${QEMU_BIN:-}}

if [[ -z $qemu_bin ]]; then
  qemu_bin=$(command -v qemu-system-x86_64 || true)
fi

[[ -n $kernel_image ]] || fail "set KERNEL_IMAGE or pass a kernel image as argument 1"
[[ -r $kernel_image ]] || fail "kernel image is not readable: $kernel_image"
[[ -r $initramfs_image ]] || fail \
  "initramfs is not readable: $initramfs_image (run demos/qemu-busybox/build-initramfs.sh)"
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
