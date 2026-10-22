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
# byte total still matches. Small by design, with no meaningful compute.
#
# The payload is 512 bytes. Every partial-transfer path still runs hundreds of
# times: 1-byte reads and writes on the pipe and the file, the 0-byte EOF read,
# and the short final block of the bs=7 copy. At 4096 bytes the run made 22,637
# syscalls and cost 18-20 CPU seconds under a debug Hermit, which went over the
# 22-second CPU budget on the hosted runner
# (https://github.com/rrnewton/hermit/issues/3337). At 512 bytes it makes 4,711
# syscalls, with the same syscall types, result classes and Hermit coverage.
# The size must be a multiple of 16, the length of one pattern repeat. One
# incidental change: `wc -c <file` reads a regular file only when its size is a
# multiple of the 4096-byte page, and otherwise takes the size from fstat, so
# the two large whole-file reads by wc are gone at 512 bytes.
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
        size=512
        # Deterministic, compressible-but-not-uniform payload.
        awk -v n=$((size / 16)) 'BEGIN { for (i = 0; i < n; i++) printf "0123456789abcdef" }' >"$src"
        src_size=$(wc -c <"$src" | tr -d '[:space:]')
        printf 'SRC %s\n' "$src_size"
        if [ "$src_size" -ne "$size" ]; then
            echo "source size mismatch: got $src_size, want $size" >&2
            exit 1
        fi

        # Byte-at-a-time through a pipe: the reader cannot get a full block, so
        # every transfer is partial.
        piped=$(cat "$src" | dd bs=1 status=none | wc -c | tr -d '[:space:]')
        printf 'PIPED %s\n' "$piped"
        if [ "$piped" -ne "$size" ]; then
            echo "pipe transfer mismatch: got $piped, want $size" >&2
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
        if [ "$copied" -ne "$size" ] || [ "$identical" != yes ]; then
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
