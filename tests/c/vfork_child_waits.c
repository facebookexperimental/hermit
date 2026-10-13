/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A vfork child that waits for a thread other than its own vfork child before
// it execs or exits (https://github.com/rrnewton/hermit/issues/4194):
// - fork-wait: the child forks a grandchild that execs /bin/true, and waits
//   for it;
// - sibling-pipe: the parent forks a writer that sleeps briefly and writes to
//   a pipe, then vforks a child that reads that pipe before it execs.
//
// Usage: vfork_child_waits fork-wait|sibling-pipe
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int fork_wait(void) {
    pid_t child = vfork();
    if (child == 0) {
        pid_t grandchild = fork();
        if (grandchild == 0) {
            execl("/bin/true", "true", (char *)NULL);
            _exit(127);
        }
        int status = 0;
        _exit(grandchild > 0 && waitpid(grandchild, &status, 0) == grandchild
                  ? WEXITSTATUS(status)
                  : 1);
    }
    int status = 0;
    if (child < 0 || waitpid(child, &status, 0) != child) {
        perror("vfork/waitpid");
        return 1;
    }
    printf("fork-wait done, child status %d\n", WEXITSTATUS(status));
    return 0;
}

static int sibling_pipe(void) {
    int fds[2];
    if (pipe(fds) != 0) {
        perror("pipe");
        return 1;
    }
    pid_t writer = fork();
    if (writer == 0) {
        struct timespec pause = {0, 50 * 1000 * 1000};
        nanosleep(&pause, NULL);
        _exit(write(fds[1], "x", 1) == 1 ? 0 : 1);
    }
    close(fds[1]);
    pid_t child = vfork();
    if (child == 0) {
        char byte;
        if (read(fds[0], &byte, 1) == 1) {
            execl("/bin/true", "true", (char *)NULL);
        }
        _exit(2);
    }
    int status = 0, writer_status = 0;
    if (child < 0 || waitpid(child, &status, 0) != child) {
        perror("vfork/waitpid");
        return 1;
    }
    if (writer < 0 || waitpid(writer, &writer_status, 0) != writer) {
        perror("fork/waitpid");
        return 1;
    }
    printf("sibling-pipe done, child status %d, writer status %d\n", WEXITSTATUS(status),
           WEXITSTATUS(writer_status));
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "fork-wait") == 0) {
        return fork_wait();
    }
    if (argc == 2 && strcmp(argv[1], "sibling-pipe") == 0) {
        return sibling_pipe();
    }
    fprintf(stderr, "usage: vfork_child_waits fork-wait|sibling-pipe\n");
    return 2;
}
