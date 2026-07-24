#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Build immutable inputs consumed by functional strict L2 probes.

set -euo pipefail

if (($# != 1)); then
    echo "usage: $0 FIXTURE_ROOT" >&2
    exit 2
fi

readonly FIXTURE_ROOT=$1
rm -rf "$FIXTURE_ROOT"
mkdir -p "$FIXTURE_ROOT/binutils" "$FIXTURE_ROOT/gprof" "$FIXTURE_ROOT/gcov"

cat >"$FIXTURE_ROOT/binutils/fixture.c" <<'EOF'
__attribute__((noinline)) int compat_line(int value) {
    return value + 1;
}
EOF
gcc -g -O0 -fno-ident -frandom-seed=hermit-binutils \
    -c "$FIXTURE_ROOT/binutils/fixture.c" \
    -o "$FIXTURE_ROOT/binutils/with-symbols.o"
/usr/bin/readelf -SW "$FIXTURE_ROOT/binutils/with-symbols.o" | grep -q '\.symtab'

cat >"$FIXTURE_ROOT/gprof/profile.c" <<'EOF'
#include <stdio.h>
volatile unsigned long sink;
__attribute__((noinline)) void compat_leaf(unsigned long value) { sink += value; }
__attribute__((noinline)) void compat_root(void) {
    for (unsigned long index = 0; index < 1000000; ++index) compat_leaf(index & 7);
}
int main(void) { compat_root(); printf("%lu\n", sink); return 0; }
EOF
gcc -O0 -pg -fno-pie -no-pie -Wl,--build-id=none \
    "$FIXTURE_ROOT/gprof/profile.c" -o "$FIXTURE_ROOT/gprof/program"
(
    cd "$FIXTURE_ROOT/gprof"
    ./program >program.out
)
test -s "$FIXTURE_ROOT/gprof/gmon.out"

cat >"$FIXTURE_ROOT/gcov/coverage.c" <<'EOF'
#include <stdio.h>
int main(void) {
    int total = 0; /* compat_marker */
    for (int index = 0; index < 5; ++index) {
        if (index % 2 == 0) total += index;
    }
    printf("%d\n", total);
    return total != 6;
}
EOF
(
    cd "$FIXTURE_ROOT/gcov"
    gcc --coverage -O0 -fno-ident -frandom-seed=hermit-gcov \
        -Wl,--build-id=none coverage.c -o coverage
    ./coverage >program.out
)
test -s "$FIXTURE_ROOT/gcov/coverage.gcno"
test -s "$FIXTURE_ROOT/gcov/coverage.gcda"

# Prevent host build time from becoming guest-visible input.
find "$FIXTURE_ROOT" -exec touch -h -d @1 {} +
printf 'prepared real compatibility fixtures: %s\n' "$FIXTURE_ROOT"
