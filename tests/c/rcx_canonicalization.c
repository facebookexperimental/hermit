/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Regression guest for rcx/r11 canonicalization (defense-in-depth determinism).
 *
 * On x86-64 the `syscall` instruction clobbers %rcx (which the CPU loads with
 * the return instruction pointer) and %r11 (the saved RFLAGS). Their contents
 * are architecturally "undefined" after a syscall, but hermit must still make
 * them deterministic even for a misbehaving guest that reads them -- and must
 * never leak Reverie's private syscall-trampoline address through %rcx.
 *
 * This program issues an `open` syscall. Detcore rewrites `open` to `openat` and
 * *injects* it from Reverie's private trampoline page, which is exactly the path
 * that used to leave the trampoline's RIP in %rcx (verified: without the fix,
 * `%rcx` differed from the return address by ~1.8 GB). The program captures
 * %rcx/%r11 immediately after the syscall and checks that %rcx equals the
 * address of the instruction right after `syscall` -- the value a faithful
 * SYSRET leaves there. Its stdout is free of absolute addresses so the line is
 * bitwise-deterministic across runs.
 */

#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    uint64_t rcx = 0;
    uint64_t r11 = 0;
    uint64_t after = 0;

    /* Raw open("/dev/null", O_RDONLY) so we can read %rcx before any other
       instruction runs. `open` is rewritten to `openat` and injected by detcore,
       exercising Reverie's trampoline path. */
    register long rax_ __asm__("rax") = SYS_open;
    register long rdi_ __asm__("rdi") = (long)"/dev/null";
    register long rsi_ __asm__("rsi") = O_RDONLY;
    register long rdx_ __asm__("rdx") = 0;
    __asm__ volatile(
        "leaq 1f(%%rip), %[after]\n\t" /* after = &(instruction after syscall) */
        "syscall\n\t"
        "1:\n\t"
        "movq %%rcx, %[rcx]\n\t"
        "movq %%r11, %[r11]\n\t"
        : "+r"(rax_), [rcx] "=r"(rcx), [r11] "=r"(r11), [after] "=r"(after)
        : "r"(rdi_), "r"(rsi_), "r"(rdx_)
        : "rcx", "r11", "cc", "memory");
    long fd = rax_;

    if (fd >= 0) {
        close((int)fd);
    }

    /* %rcx must hold the return RIP. A leaked trampoline address would differ. */
    if (rcx != after) {
        fprintf(stderr,
                "FAIL: rcx (0x%llx) != return RIP (0x%llx): non-canonical %%rcx "
                "(delta=%lld)\n",
                (unsigned long long)rcx, (unsigned long long)after,
                (long long)(rcx - after));
        return 1;
    }

    /* %r11 holds RFLAGS; bit 1 is the reserved, always-set bit. */
    if ((r11 & 0x2) == 0) {
        fprintf(stderr, "FAIL: r11 (0x%llx) does not look like RFLAGS\n",
                (unsigned long long)r11);
        return 1;
    }

    /* Deterministic output: report the invariants, never the absolute values. */
    printf("rcx_is_return_rip=1 r11_reserved_bit=%d open_ok=%d\n",
           (int)((r11 & 0x2) != 0), (int)(fd >= 0));
    return 0;
}
