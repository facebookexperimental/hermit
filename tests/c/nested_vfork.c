/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A vfork child that vforks again and execs, then exits: the double spawn a
// daemonizing helper does (a git wrapper's telemetry helper did this, and a
// Linux kernel build that ran git hung under Hermit,
// https://github.com/rrnewton/hermit/issues/4163).
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    pid_t child = vfork();
    if (child == 0) {
        pid_t grandchild = vfork();
        if (grandchild == 0) {
            execl("/bin/true", "true", (char *)NULL);
            _exit(127);
        }
        _exit(grandchild > 0 ? 0 : 1);
    }
    int status = 0;
    if (child < 0 || waitpid(child, &status, 0) != child) {
        perror("vfork/waitpid");
        return 1;
    }
    printf("nested vfork done, child status %d\n", WEXITSTATUS(status));
    return 0;
}
