#!/usr/bin/env bash

set -euo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/../.." && pwd)
hermit_bin=${HERMIT_BIN:-$repo_root/target/release/hermit}
kernel_image=${KERNEL_IMAGE:-/boot/vmlinuz}
output_dir=${OUTPUT_DIR:-$repo_root/target/qemu-boot-smoke}
initramfs_image=${INITRAMFS_IMAGE:-$output_dir/initramfs.cpio.gz}
timeout_seconds=${QEMU_BOOT_TIMEOUT_SECONDS:-90}
qemu_bin=${QEMU_BIN:-}

if [[ -z $qemu_bin ]]; then
  qemu_bin=$(command -v qemu-system-x86_64 || true)
fi

if [[ ! -x $hermit_bin ]]; then
  fail "Hermit release binary not found: $hermit_bin (run cargo build --release)"
fi
if [[ -z $qemu_bin || ! -x $qemu_bin ]]; then
  fail "qemu-system-x86_64 not found; set QEMU_BIN"
fi
[[ -r $kernel_image ]] || fail "kernel image is not readable: $kernel_image"
if [[ ! $timeout_seconds =~ ^[1-9][0-9]*$ ]]; then
  fail "QEMU_BOOT_TIMEOUT_SECONDS must be a positive integer"
fi

for command in cpio gcc gzip grep sed timeout; do
  command -v "$command" >/dev/null || fail "required command not found: $command"
done

init_source=$repo_root/experiments/shared-futex-verify_20260722/qemu_init.c
initramfs_root=$output_dir/initramfs-root
console_log=$output_dir/console.log
[[ -r $init_source ]] || fail "init source is not readable: $init_source"
mkdir -p "$initramfs_root" "$(dirname -- "$initramfs_image")"

gcc -Os -nostdlib -static -fno-stack-protector -fno-pie -no-pie \
  "$init_source" \
  -o "$initramfs_root/init"
(
  cd "$initramfs_root"
  printf '.\n./init\n' | cpio --quiet -o -H newc
) | gzip -9 >"$initramfs_image"

printf 'kernel=%s\ninitramfs=%s\nconsole=%s\n' \
  "$kernel_image" "$initramfs_image" "$console_log"

set +e
timeout --signal=KILL "${timeout_seconds}s" \
  "$hermit_bin" --log error run \
  --no-sequentialize-threads \
  --preemption-timeout disabled \
  --no-virtualize-cpuid -- \
  "$qemu_bin" \
  -m 256M \
  -accel tcg,thread=single \
  -smp 1 \
  -icount shift=0,sleep=off \
  -kernel "$kernel_image" \
  -initrd "$initramfs_image" \
  -display none \
  -serial stdio \
  -monitor none \
  -no-reboot \
  -append 'console=ttyS0 panic=-1 rdinit=/init' \
  >"$console_log" 2>&1
status=$?
set -e

if [[ $status -ne 0 ]]; then
  printf 'QEMU boot exited with status %s. Console follows:\n' "$status" >&2
  sed -n '1,240p' "$console_log" >&2
  exit 1
fi

marker=SHARED_FUTEX_QEMU_KERNEL_OK
grep -F "$marker" "$console_log" >/dev/null || {
  printf 'QEMU exited successfully without marker %s. Console follows:\n' \
    "$marker" >&2
  sed -n '1,240p' "$console_log" >&2
  exit 1
}

clock_failures='Unable to calibrate against PIT|Clocksource .* skewed|Marking TSC unstable|No current clocksource'
if grep -E "$clock_failures" "$console_log" >/dev/null; then
  printf 'QEMU boot reached a rejected clock failure. Console follows:\n' >&2
  sed -n '1,240p' "$console_log" >&2
  exit 1
fi

grep -F "$marker" "$console_log"
printf 'QEMU boot smoke test passed.\n'
