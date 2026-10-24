#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

case ${1:-} in
    --prepare)
        for tool in ar cc sha256sum; do
            command -v "$tool" >/dev/null 2>&1 || {
                echo "$tool not found" >&2
                exit 1
            }
        done
        ;;
    --run)
        work="${E2E_TMPDIR:-/tmp}/hermit-c-toolchain-workflow"
        rm -rf -- "$work"
        mkdir -p -- "$work"

        cat >"$work/calc.h" <<'EOF'
#ifndef HERMETIC_CALC_H
#define HERMETIC_CALC_H
long weighted_sum(const int *values, int count);
#endif
EOF
        cat >"$work/calc.c" <<'EOF'
#include "calc.h"

long weighted_sum(const int *values, int count) {
    long result = 0;
    for (int i = 0; i < count; ++i) {
        result += (long)(i + 1) * values[i];
    }
    return result;
}
EOF
        cat >"$work/main.c" <<'EOF'
#include "calc.h"

#include <stdio.h>

int main(int argc, char **argv) {
    if (argc != 2) {
        return 2;
    }
    FILE *input = fopen(argv[1], "r");
    if (input == NULL) {
        return 3;
    }
    int values[16];
    int count = 0;
    while (count < 16 && fscanf(input, "%d", &values[count]) == 1) {
        ++count;
    }
    if (fclose(input) != 0 || count == 0) {
        return 4;
    }
    printf("TOOLCHAIN count=%d weighted_sum=%ld\n", count,
           weighted_sum(values, count));
    return 0;
}
EOF
        printf '3\n1\n4\n1\n5\n9\n' >"$work/input.txt"

        cc -std=c11 -O2 -g0 -Wall -Wextra -Werror -fno-ident \
            -I"$work" -c "$work/calc.c" -o "$work/calc.o"
        ar rcsD "$work/libcalc.a" "$work/calc.o"
        cc -std=c11 -O2 -g0 -Wall -Wextra -Werror -fno-ident \
            -I"$work" -c "$work/main.c" -o "$work/main.o"
        cc -Wl,--build-id=none "$work/main.o" "$work/libcalc.a" -o "$work/toolchain-app"

        "$work/toolchain-app" "$work/input.txt"
        printf 'TOOLCHAIN archive_sha256=%s\n' \
            "$(sha256sum "$work/libcalc.a" | cut -d' ' -f1)"
        printf 'TOOLCHAIN binary_sha256=%s\n' \
            "$(sha256sum "$work/toolchain-app" | cut -d' ' -f1)"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
