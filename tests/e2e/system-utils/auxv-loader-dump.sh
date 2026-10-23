#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

# Surface: the AUXILIARY VECTOR, which no corpus entry read.
#
# LD_SHOW_AUXV makes glibc's dynamic loader walk and print the auxv it was
# handed by execve. That is real loader code on the real startup path, not a
# --version probe, and it is the only entry that observes AT_RANDOM (the 16
# bytes glibc seeds stack-protector and malloc from), AT_SYSINFO_EHDR (the vdso
# base), AT_HWCAP/AT_HWCAP2, AT_SECURE, AT_CLKTCK and AT_PAGESZ.
#
# Deliberately UNNORMALIZED. AT_RANDOM is an entropy source and AT_SYSINFO_EHDR
# is an address; printing them raw is the point, because a backend that leaks
# host entropy or a host vdso placement into the guest diverges here and
# nowhere else in the corpus. Values are host-specific, so native comparison is
# not a contract -- the naked mode stays disabled.
case ${1:-} in
    --prepare) exit 0 ;;
    --run)
        # /bin/true is the smallest real dynamically-linked binary; the work
        # under test is the loader's, not the program's.
        work="${E2E_TMPDIR:-/tmp}"
        mkdir -p "$work"
        LD_SHOW_AUXV=1 /bin/true 2>&1 | sort >"$work/auxv.txt"

        # The whole dump, verbatim: every key and value must repeat exactly.
        printf 'AUXV-DUMP\n'
        cat "$work/auxv.txt"

        # Structural checks make a truncated or empty dump fail, rather than
        # merely producing the same incomplete output twice.
        keys=$(grep -c '^AT_' "$work/auxv.txt" || true)
        printf 'AUXV-KEYS %s\n' "$keys"
        if [ "$keys" -lt 5 ]; then
            echo "auxv dump is incomplete: found $keys AT_ entries" >&2
            exit 1
        fi
        for key in AT_RANDOM AT_SYSINFO_EHDR AT_PAGESZ AT_SECURE AT_CLKTCK; do
            matches=$(grep -c "^$key:" "$work/auxv.txt" || true)
            printf 'HAS %s %s\n' "$key" "$matches"
            if [ "$matches" -ne 1 ]; then
                echo "auxv key $key occurred $matches times, want exactly once" >&2
                exit 1
            fi
        done
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
