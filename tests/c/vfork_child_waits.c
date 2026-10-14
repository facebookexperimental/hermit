/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A vfork child that waits before it execs or exits
// (https://github.com/rrnewton/hermit/issues/4194):
// - fork-wait: the child forks a grandchild that execs /bin/true, and waits
//   for it;
// - sibling-pipe: the parent forks a writer that sleeps briefly and writes to
//   a pipe, then vforks a child that reads that pipe before it execs;
// - sibling-poll, sibling-poll-timeout: as sibling-pipe, but the child first
//   polls the pipe, with no timeout or with a 5 s one. Under `hermit record`
//   that poll runs in the background, and the writer must still run
//   (https://github.com/rrnewton/hermit/issues/4211);
// - background-sibling: a sibling thread of the parent spins while the vfork
//   child makes calls whose completion the host times (poll with a timeout),
//   and the child reports how far the sibling got meanwhile, which must be the
//   same in every run (Claude review of
//   https://github.com/rrnewton/hermit/pull/4205).
//
// A status is printed only for a child that exited; a child killed by a
// signal is reported as such.
//
// Usage: vfork_child_waits
//   fork-wait|sibling-pipe|sibling-poll|sibling-poll-timeout|background-sibling
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

// The exit status of `pid`, or -1 if it did not exit normally (killed by a
// signal) or could not be waited for.
static int exit_status(pid_t pid) {
    int status = 0;
    if (pid < 0 || waitpid(pid, &status, 0) != pid || !WIFEXITED(status)) {
        return -1;
    }
    return WEXITSTATUS(status);
}

static int fork_wait(void) {
    pid_t child = vfork();
    if (child == 0) {
        pid_t grandchild = fork();
        if (grandchild == 0) {
            execl("/bin/true", "true", (char *)NULL);
            _exit(127);
        }
        int grandchild_status = exit_status(grandchild);
        _exit(grandchild_status < 0 ? 1 : grandchild_status);
    }
    printf("fork-wait done, child status %d\n", exit_status(child));
    return 0;
}

// `poll_ms` < -1 reads the pipe at once; otherwise the child polls it first
// with that timeout (-1: none).
static int sibling_pipe_mode(const char *name, int poll_ms) {
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
        if (poll_ms >= -1) {
            struct pollfd readable = {fds[0], POLLIN, 0};
            int ready = poll(&readable, 1, poll_ms);
            if (ready != 1) {
                _exit(ready == 0 ? 4 : 3);
            }
        }
        char byte = 0;
        if (read(fds[0], &byte, 1) == 1 && byte == 'x') {
            execl("/bin/true", "true", (char *)NULL);
        }
        _exit(2);
    }
    int child_status = exit_status(child);
    printf("%s done, child status %d, writer status %d\n", name, child_status,
           exit_status(writer));
    return 0;
}

static volatile long progress;
static volatile int stop;

static void *spin(void *unused) {
    (void)unused;
    while (!stop) {
        progress++;
    }
    return NULL;
}

static int background_sibling(void) {
    static volatile long sibling_progress = -1;
    pthread_t sibling;
    if (pthread_create(&sibling, NULL, spin, NULL) != 0) {
        perror("pthread_create");
        return 1;
    }
    pid_t child = vfork();
    if (child == 0) {
        long before = progress;
        for (int i = 0; i < 10; i++) {
            if (poll(NULL, 0, 20) < 0) {
                _exit(3);
            }
        }
        sibling_progress = progress - before;
        _exit(0);
    }
    int child_status = exit_status(child);
    stop = 1;
    pthread_join(sibling, NULL);
    printf("background-sibling done, child status %d, sibling progress while the child waited %ld\n",
           child_status, sibling_progress);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "fork-wait") == 0) {
        return fork_wait();
    }
    if (argc == 2 && strcmp(argv[1], "sibling-pipe") == 0) {
        return sibling_pipe_mode("sibling-pipe", -2);
    }
    if (argc == 2 && strcmp(argv[1], "sibling-poll") == 0) {
        return sibling_pipe_mode("sibling-poll", -1);
    }
    if (argc == 2 && strcmp(argv[1], "sibling-poll-timeout") == 0) {
        return sibling_pipe_mode("sibling-poll-timeout", 5000);
    }
    if (argc == 2 && strcmp(argv[1], "background-sibling") == 0) {
        return background_sibling();
    }
    fprintf(stderr, "usage: vfork_child_waits "
                    "fork-wait|sibling-pipe|sibling-poll|sibling-poll-timeout|"
                    "background-sibling\n");
    return 2;
}
