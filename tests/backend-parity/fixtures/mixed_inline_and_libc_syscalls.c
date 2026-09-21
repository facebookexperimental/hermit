/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * MIXED guest: patched and unpatched syscall paths in a SINGLE run.
 *
 * The inline guest is all-patchable and the freestanding guest is all-in-main-ELF.
 * Neither exercises the case that actually worries a rewriter: one process where
 * SOME syscalls go through rewritten inline sites and others go through libc.so,
 * which the rewriter never touched. Both paths must reach the same tool and be
 * ordered coherently; a backend that intercepts only one of them, or that
 * interleaves them inconsistently, is wrong in a way neither single-path guest
 * can show.
 *
 * The two paths are deliberately INTERLEAVED and both write to stdout, so their
 * relative order is observable: if the patched path were buffered, dropped, or
 * reordered against the libc path, the output changes.
 *
 * DETERMINISM. Fixed strings, fixed iteration count, an order-sensitive checksum
 * over syscall return values. stdout is flushed before each inline write so the
 * interleaving is defined by the program rather than by libc's buffer state --
 * without that the ordering would depend on buffer size, which is not something
 * this test means to pin.
 */

#include <stdio.h>
#include <string.h>
#include <unistd.h>

static long sys_write_inline(int fd, const void *buf, unsigned long len) {
    long ret;
    __asm__ volatile("syscall"
                     : "=a"(ret)
                     : "a"(1L), "D"((long)fd), "S"(buf), "d"(len)
                     : "rcx", "r11", "memory");
    return ret;
}

int main(void) {
    unsigned long checksum = 0;

    for (int i = 0; i < 4; i++) {
        /* libc path: printf -> PLT -> libc.so, NOT visible to a main-ELF rewriter. */
        printf("libc  path iteration=%d\n", i);
        /* Flush so the inline write below cannot land before this line. */
        if (fflush(stdout) != 0) {
            return 2;
        }

        /* Inline path: a syscall instruction inside the main ELF, which IS. */
        static const char msg[] = "inline path iteration\n";
        long w = sys_write_inline(1, msg, sizeof(msg) - 1);
        if (w != (long)(sizeof(msg) - 1)) {
            return 2;
        }
        checksum = checksum * 131u + (unsigned long)w;

        /* libc path again, this time a raw write(2) rather than stdio, so the
         * comparison is inline-vs-libc and not inline-vs-buffered-stdio. */
        static const char via_libc[] = "libc  path raw write\n";
        ssize_t r = write(1, via_libc, sizeof(via_libc) - 1);
        if (r != (ssize_t)(sizeof(via_libc) - 1)) {
            return 2;
        }
        checksum = checksum * 131u + (unsigned long)r;
    }

    printf("mixed_paths checksum=%lu\n", checksum);
    fflush(stdout);
    return 0;
}
