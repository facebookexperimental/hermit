/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * FREESTANDING STATIC guest: no PLT, no interpreter, no libc.so.
 *
 * Built `-static -nostdlib -nostartfiles`, so the binary has no dynamic loader,
 * no PLT, and no shared libc. EVERY instruction it executes -- including every
 * syscall instruction -- lives in the main ELF. That is the whole address space
 * a main-ELF rewriter is responsible for, with nothing hidden behind a PLT thunk
 * into libc.so, so a rewriter that misses sites here has nowhere to hide.
 *
 * WHY NOT `-static` WITH GLIBC. That would be the ideal shape -- libc's OWN
 * syscall instructions linked into the main ELF, a program form people actually
 * ship. It is not buildable on this host: glibc's static archive is absent
 * (`/usr/bin/ld: have you installed the static version of the c library ?`,
 * no /usr/lib64/libc.a, no musl-gcc). That variant is therefore NOT covered
 * here; adding it needs `glibc-static` (or a musl toolchain) on the builder.
 * This guest covers the no-PLT/whole-program-in-main-ELF half of that case,
 * which is the half a rewriter's reach actually depends on.
 *
 * DETERMINISM. Fixed writes and an order-sensitive checksum folded from syscall
 * return values. No clock, pid, environment, filesystem enumeration, or
 * randomness. Freestanding means there is no libc buffering to order, so output
 * order is exactly the order of the write syscalls.
 */

typedef long ssize_t_;

static long sys3(long nr, long a, long b, long c) {
    long ret;
    __asm__ volatile("syscall"
                     : "=a"(ret)
                     : "a"(nr), "D"(a), "S"(b), "d"(c)
                     : "rcx", "r11", "memory");
    return ret;
}

static long sys0(long nr) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr) : "rcx", "r11", "memory");
    return ret;
}

static unsigned long slen(const char *s) {
    unsigned long n = 0;
    while (s[n] != '\0') {
        n++;
    }
    return n;
}

static long emit(const char *s) {
    return sys3(1 /*write*/, 1 /*stdout*/, (long)s, (long)slen(s));
}

/* Decimal rendering without libc, so the checksum is printable. */
static void emit_ulong(unsigned long v) {
    char buf[24];
    int i = (int)sizeof(buf);
    buf[--i] = '\n';
    if (v == 0) {
        buf[--i] = '0';
    }
    while (v > 0) {
        buf[--i] = (char)('0' + (v % 10));
        v /= 10;
    }
    sys3(1, 1, (long)&buf[i], (long)((int)sizeof(buf) - i));
}

void _start(void) {
    unsigned long checksum = 0;

    /* Several distinct, repeatedly executed sites. A rewriter must patch a hot
     * site re-entrantly, not merely find it once. */
    for (int i = 0; i < 6; i++) {
        long w = emit("freestanding: site\n");
        checksum = checksum * 131u + (unsigned long)w;
        long y = sys0(24 /*sched_yield*/);
        checksum = checksum * 131u + (unsigned long)y;
    }

    /* A site whose result is deliberately discarded and never printed. */
    (void)sys0(39 /*getpid*/);

    emit("freestanding: checksum=");
    emit_ulong(checksum);

    sys3(231 /*exit_group*/, 0, 0, 0);
    /* exit_group does not return; loop keeps the compiler's noreturn analysis
     * happy without pulling in libc's __builtin_unreachable machinery. */
    for (;;) {
    }
}
