/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A chain of vfork children, each vforking the next, whose last child execs;
// every other child then exits. Depth 2 is the double spawn a daemonizing
// helper does (a git wrapper's telemetry helper did this, and a Linux kernel
// build that ran git hung under Hermit,
// https://github.com/rrnewton/hermit/issues/4163).
//
// Usage: nested_vfork [DEPTH], where DEPTH >= 1 (default 2) counts the vforks.
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

// Runs in a vfork child, so it never returns: it vforks `depth` more times
// down the chain, the last child execs, and each child exits.
static void __attribute__((noreturn)) spawn_chain(int depth) {
    if (depth == 0) {
        execl("/bin/true", "true", (char *)NULL);
        _exit(127);
    }
    pid_t child = vfork();
    if (child == 0) {
        spawn_chain(depth - 1);
    }
    _exit(child > 0 ? 0 : 1);
}

int main(int argc, char **argv) {
    int depth = argc > 1 ? atoi(argv[1]) : 2;
    if (depth < 1) {
        fprintf(stderr, "usage: nested_vfork [DEPTH >= 1]\n");
        return 2;
    }
    pid_t child = vfork();
    if (child == 0) {
        spawn_chain(depth - 1);
    }
    int status = 0;
    if (child < 0 || waitpid(child, &status, 0) != child) {
        perror("vfork/waitpid");
        return 1;
    }
    printf("nested vfork depth %d done, child status %d\n", depth, WEXITSTATUS(status));
    return 0;
}
