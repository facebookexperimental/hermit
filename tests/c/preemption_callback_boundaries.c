/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Linux x86_64, Hermit's supported host ABI. No libc, loop or branch. */
#if !defined(__linux__) || !defined(__x86_64__)
#error "This direct-syscall regression requires Linux x86_64"
#endif

static __attribute__((always_inline)) inline long call3(long n, long a, long b, long c) {
    long result;
    __asm__ volatile ("syscall" : "=a"(result) : "0"(n), "D"(a), "S"(b), "d"(c)
                      : "rcx", "r11", "memory");
    return result;
}

#define CLOCK() (failed |= call3(228, 1, (long)&now, 0))
#define C2() CLOCK(); CLOCK()
#define C4() C2(); C2()
#define C8() C4(); C4()
#define C16() C8(); C8()
#define C32() C16(); C16()
#define C64() C32(); C32()

__attribute__((noreturn)) void _start(void) {
    struct { long seconds, nanoseconds; } now;
    long failed = 0;
    C64();
    long wrote = call3(1, 1, (long)"CLOCK_OK\n", 9);
    call3(60, (failed != 0) | (wrote != 9), 0, 0);
    __builtin_unreachable();
}
