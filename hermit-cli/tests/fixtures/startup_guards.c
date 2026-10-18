/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/auxv.h>

#if !defined(__linux__) || !defined(__x86_64__) || !defined(__GLIBC__)
#error "This probe requires native x86_64 Linux GNU libc startup"
#endif

int main(void) {
    uint64_t canary, pointer_guard;
    __asm__ volatile("movq %%fs:0x28, %0" : "=r"(canary));
    __asm__ volatile("movq %%fs:0x30, %0" : "=r"(pointer_guard));
    const unsigned char *random = (const unsigned char *)getauxval(AT_RANDOM);
    if (random == NULL) {
        fputs("missing AT_RANDOM\n", stderr);
        return 2;
    }
    printf("canary=%016" PRIx64 " pointer_guard=%016" PRIx64 " at_random=",
           canary, pointer_guard);
    for (unsigned i = 0; i < 16; ++i) {
        printf("%02x", (unsigned)random[i]);
    }
    putchar('\n');
    return ferror(stdout) ? 3 : 0;
}
