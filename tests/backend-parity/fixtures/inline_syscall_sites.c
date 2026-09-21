/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Inline-syscall guest: gives a MAIN-ELF PATCHER SOMETHING TO PATCH.
 *
 * WHY THIS EXISTS. A main-ELF rewriter (e9patch, and the same reach question
 * applies to SaBRe and LiteInst) discovers syscall sites by scanning the main
 * executable. An ordinary dynamically-linked C program contains NO `syscall`
 * instruction of its own: it calls into libc through the PLT, and libc's
 * syscall instructions live in libc.so, outside the rewriter's reach. Measured
 * on this corpus, a typical guest yields `candidate_sites=0; mapped_sites=0`.
 *
 * The consequence is that a patching backend running such a guest performs NO
 * patching at all -- it degrades to the plain ptrace runtime and then scores
 * byte-identical parity against the ptrace reference. That is a manufactured
 * 100%, not a measurement: the backend under test was never exercised.
 *
 * This guest closes that hole by issuing syscalls from inline assembly in the
 * main ELF, so `mapped_sites > 0` and the patcher is actually on the path.
 *
 * THIS IS NOT A PATCHING-BACKEND-SPECIFIC TEST. It is an ordinary deterministic
 * guest that every backend runs; ptrace is the reference arm as usual. The only
 * thing that makes it special is that the bytes a rewriter needs are present.
 *
 * DETERMINISM. Output is a fixed sequence of writes plus a checksum folded from
 * the syscall return values (byte counts), which are themselves fixed. Nothing
 * reads the clock, the pid, the environment, or any host state, so the guest is
 * a pure function of its own code under any backend.
 */

#include <stdio.h>
#include <string.h>

/* One inline syscall site. `volatile` keeps it from being optimized away or
 * merged with its neighbours, so the site count is a property of the source. */
static long sys_write(int fd, const void *buf, unsigned long len) {
    long ret;
    __asm__ volatile("syscall"
                     : "=a"(ret)
                     : "a"(1L), "D"((long)fd), "S"(buf), "d"(len)
                     : "rcx", "r11", "memory");
    return ret;
}

static long sys_getpid_discarded(void) {
    /* A second DISTINCT site, and a syscall whose result we deliberately do not
     * print: a patcher must handle sites whose values never reach stdout, and a
     * pid is not deterministic so printing it would be a determinism bug. */
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(39L) : "rcx", "r11", "memory");
    return ret;
}

static long sys_sched_yield(void) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(24L) : "rcx", "r11", "memory");
    return ret;
}

int main(void) {
    static const char line0[] = "inline: first site\n";
    static const char line1[] = "inline: loop sites\n";

    /* Site 1. */
    long total = sys_write(1, line0, sizeof(line0) - 1);

    /* Sites 2 and 3, exercised repeatedly: a rewriter must patch a site that is
     * executed many times, not just once, and the trampoline must be re-entrant. */
    total += sys_write(1, line1, sizeof(line1) - 1);
    for (int i = 0; i < 8; i++) {
        total += sys_write(1, "", 0); /* 0-byte write: valid, returns 0 */
        if (sys_sched_yield() != 0) {
            return 2;
        }
    }

    /* A site whose value is intentionally discarded. */
    if (sys_getpid_discarded() <= 0) {
        return 2;
    }

    /* Deterministic: the byte counts above are fixed by the string lengths. */
    printf("inline_sites total_written=%ld\n", total);
    return 0;
}
