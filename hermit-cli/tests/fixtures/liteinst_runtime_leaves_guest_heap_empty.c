/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * In-guest LiteInst installs the Detcore Tool from a constructor that runs
 * before this program's main. Anything that installation allocated through
 * this process's C library heap would still be there when main starts, and
 * the program's own allocations would be placed around it. Its sizes follow
 * host state (the Tool's configuration lists every mount of the guest's
 * namespace), so the program's heap addresses, which its syscalls pass to
 * the kernel, would follow host state too. This prints the C library's heap
 * at main's entry, before the program allocates anything, and where its first
 * allocation lands.
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <malloc.h>
#include <stdio.h>
#include <stdlib.h>

int main(void) {
    struct mallinfo2 before = mallinfo2();
    void *first = malloc(1);
    if (first == NULL) {
        return 1;
    }
    /* How often a preloaded libc wrapper ran before main, when the test
     * preloads one that counts its calls; 0 otherwise. */
    const unsigned *calls = dlsym(RTLD_DEFAULT, "reverie_test_interposer_calls");
    printf("heap at main: arena=%zu in_use=%zu\n", before.arena, before.uordblks);
    printf("first allocation: %p\n", first);
    printf("interposed calls before main: %u\n", calls == NULL ? 0 : *calls);
    free(first);
    return 0;
}
