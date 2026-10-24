/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * In-guest LiteInst rebuilds a fork child's runtime state (its branch
 * counter among it) from the fork's own return path, inside the child, before
 * the child's next line runs. Anything that rebuild allocated through the
 * child's C library heap would be placed before the child's own allocations,
 * and whether the counter can be bound at all depends on the host. This prints
 * the child's heap after one syscall and where its first allocation lands,
 * and the parent's after the child exits, using write() and stack buffers so
 * that stdio allocates nothing.
 */

#include <malloc.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

static void say(const char *who, void *first) {
    struct mallinfo2 heap = mallinfo2();
    char line[160];
    int length = snprintf(
        line, sizeof line, "%s: arena=%zu in_use=%zu first=%p\n", who, heap.arena,
        heap.uordblks, first);
    if (write(1, line, (size_t)length) != length) {
        _exit(2);
    }
}

int main(void) {
    say("parent before fork", NULL);
    pid_t child = fork();
    if (child < 0) {
        return 1;
    }
    if (child == 0) {
        (void)getppid();
        struct mallinfo2 heap = mallinfo2();
        void *first = malloc(1);
        char line[160];
        int length = snprintf(
            line, sizeof line, "child: arena=%zu in_use=%zu first=%p\n", heap.arena,
            heap.uordblks, first);
        _exit(write(1, line, (size_t)length) == length ? 0 : 2);
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return 3;
    }
    char line[64];
    int length = snprintf(line, sizeof line, "child status=%d\n", status);
    if (write(1, line, (size_t)length) != length) {
        return 2;
    }
    say("parent after wait", malloc(1));
    return 0;
}
