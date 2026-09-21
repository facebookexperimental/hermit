#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

# Surface: PARTIAL-TRANSFER file I/O in a REAL program.
#
# The corpus exercised partial transfers only through purpose-built C fixtures.
# dd bs=1 across a pipe is the coreutils-native version of the same surface:
# every byte is its own read()/write() pair, so a backend that coalesces or
# splits transfers differently produces a different syscall stream while the
# byte total still matches. Small by design -- 4096 bytes is ~8k syscalls and
# no meaningful compute.
#
# dd's own stats line is suppressed with status=none: it reports a virtual-time
# derived rate, which the time-focused entries already cover, and leaving it in
# would put a second observable in this entry's oracle for no added surface.
case ${1:-} in
    --prepare) exit 0 ;;
    --run)
        work="${E2E_TMPDIR:-/tmp}/dd-partial"
        rm -rf "$work"; mkdir -p "$work"
        src="$work/src.bin"
        # Deterministic, compressible-but-not-uniform 4096-byte payload.
        awk 'BEGIN { for (i = 0; i < 256; i++) printf "0123456789abcdef" }' >"$src"
        src_size=$(wc -c <"$src" | tr -d '[:space:]')
        printf 'SRC %s\n' "$src_size"
        if [ "$src_size" -ne 4096 ]; then
            echo "source size mismatch: got $src_size, want 4096" >&2
            exit 1
        fi

        # Byte-at-a-time through a pipe: the reader cannot get a full block, so
        # every transfer is partial.
        piped=$(cat "$src" | dd bs=1 status=none | wc -c | tr -d '[:space:]')
        printf 'PIPED %s\n' "$piped"
        if [ "$piped" -ne 4096 ]; then
            echo "pipe transfer mismatch: got $piped, want 4096" >&2
            exit 1
        fi

        # Byte-at-a-time to a file, then compare content to prove no byte was
        # dropped or duplicated by the split.
        dd if="$src" of="$work/out.bin" bs=1 status=none
        copied=$(wc -c <"$work/out.bin" | tr -d '[:space:]')
        if cmp -s "$src" "$work/out.bin"; then
            identical=yes
        else
            identical=no
        fi
        printf 'COPIED %s\n' "$copied"
        printf 'IDENTICAL %s\n' "$identical"
        if [ "$copied" -ne 4096 ] || [ "$identical" != yes ]; then
            echo "file transfer mismatch: copied=$copied identical=$identical" >&2
            exit 1
        fi

        # A short odd-sized block count exercises the final partial block.
        odd=$(head -c 89 "$src" | dd bs=7 count=13 status=none | wc -c | tr -d '[:space:]')
        printf 'ODDBLOCK %s\n' "$odd"
        if [ "$odd" -ne 89 ]; then
            echo "partial block mismatch: got $odd, want 89" >&2
            exit 1
        fi
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
