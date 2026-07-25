#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

here=$(cd -- "$(dirname -- "$0")" && pwd)
out=${1:-/tmp/shared-futex-verify_20260722}

mkdir -p "$out/classes" "$out/initramfs-root"

gcc -O2 -pthread \
  "$here/pthread_futex.c" \
  -o "$out/pthread_futex"

javac -d "$out/classes" "$here/Threaded.java"
jar cfe "$out/threaded.jar" Threaded -C "$out/classes" .

gcc -Os -nostdlib -static -fno-stack-protector -fno-pie -no-pie \
  "$here/qemu_init.c" \
  -o "$out/initramfs-root/init"

(
  cd "$out/initramfs-root"
  printf '.\n./init\n' | cpio -o -H newc
) | gzip -9 > "$out/initramfs.cpio.gz"

printf 'assets=%s\n' "$out"
