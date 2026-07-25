#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# AUTONOMOUS-BOT-IMPLEMENTED
# TODO-HUMAN-REVIEW(#701): Review the configure/build shell composition fixture.
# Configure, build, link, and inspect a freestanding program through shell composition.

set -euo pipefail
export LC_ALL=C
export PATH=/usr/bin:/bin

readonly WORK_DIR=/tmp/hermit-complex-shell-workload
readonly SRC_DIR="$WORK_DIR/src"
readonly BUILD_DIR="$WORK_DIR/build"

rm -rf "$WORK_DIR"
mkdir -p "$SRC_DIR" "$BUILD_DIR"
trap 'rm -rf "$WORK_DIR"' EXIT

for tool in /usr/bin/gcc /usr/bin/as /usr/bin/ld /usr/bin/nm; do
    test -x "$tool"
done
test "$(/usr/bin/uname -m)" = x86_64

cat >"$SRC_DIR/conftest.c" <<'EOF'
#include <stdint.h>
_Static_assert(sizeof(uint32_t) == 4, "uint32_t must be four bytes");
int probe(uint32_t value) { return (int)(value + 1); }
EOF

if (
    cd "$BUILD_DIR"
    /usr/bin/gcc -std=c11 -Wall -Werror -ffreestanding -fno-ident \
        -frandom-seed=hermit-configure -c "$SRC_DIR/conftest.c" \
        -o conftest.o
) >"$BUILD_DIR/config.log" 2>&1; then
    have_stdint=1
else
    have_stdint=0
fi
test "$have_stdint" -eq 1

{
    printf '#define HAVE_STDINT_H %s\n' "$have_stdint"
    printf '#define CONFIG_BIAS 0\n'
} | /usr/bin/sort >"$BUILD_DIR/config.h"

/usr/bin/diff -u \
    <(printf '%s\n' '#define CONFIG_BIAS 0' '#define HAVE_STDINT_H 1') \
    "$BUILD_DIR/config.h"

cat >"$SRC_DIR/answer.c" <<'EOF'
#include <stdint.h>
#include "config.h"

#if HAVE_STDINT_H != 1
#error "configure probe did not find stdint.h"
#endif

int shell_answer(uint32_t left, uint32_t right) {
    return (int)(left * right) + CONFIG_BIAS;
}
EOF

cat >"$SRC_DIR/start.s" <<'EOF'
    .text
    .globl _start
    .extern shell_answer
_start:
    mov $6, %edi
    mov $7, %esi
    call shell_answer
    cmp $42, %eax
    jne .Lfailure
    mov $1, %eax
    mov $1, %edi
    lea message(%rip), %rsi
    mov $15, %edx
    syscall
    xor %edi, %edi
    jmp .Lexit
.Lfailure:
    mov %eax, %edi
.Lexit:
    mov $60, %eax
    syscall

    .section .rodata
message:
    .ascii "shell-build=42\n"
    .section .note.GNU-stack,"",@progbits
EOF

(
    cd "$BUILD_DIR"
    SOURCE_DATE_EPOCH=946684800 /usr/bin/gcc -std=c11 -O2 -Wall -Werror \
        -ffreestanding -fno-ident -frandom-seed=hermit-shell-build \
        -fno-stack-protector -fno-pie -I. -c "$SRC_DIR/answer.c" \
        -o answer.o
    /usr/bin/as --64 "$SRC_DIR/start.s" -o start.o
    /usr/bin/ld --build-id=none -o program start.o answer.o
)

output=$("$BUILD_DIR/program")
test "$output" = 'shell-build=42'

/usr/bin/nm -g --defined-only "$BUILD_DIR/program" >"$BUILD_DIR/symbols"
symbols=$(/usr/bin/awk \
    '$3 == "_start" || $3 == "shell_answer" { print $3 }' \
    "$BUILD_DIR/symbols")
/usr/bin/diff -u \
    <(printf '%s\n' _start shell_answer) \
    <(printf '%s\n' "$symbols")

manifest=$(
    printf '%s\n' configure compile link run |
        /usr/bin/sed 's/^/step:/'
)
test "$manifest" = $'step:configure\nstep:compile\nstep:link\nstep:run'

printf 'complex-shell:configure-build-run-ok\n'
