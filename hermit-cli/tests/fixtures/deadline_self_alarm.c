/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// https://github.com/rrnewton/hermit/issues/4121: the running thread arms a
// one-shot 5 ms ITIMER_REAL, then spins (on getppid, or with no syscalls when
// argv[1] is "pure") until the handler sets a flag. Prints how much
// CLOCK_MONOTONIC time the signal took, in microseconds.
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t fired = 0;

static void on_alarm(int signo) {
    (void)signo;
    fired = 1;
}

int main(int argc, char **argv) {
    int pure = argc > 1 && strcmp(argv[1], "pure") == 0;
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_handler = on_alarm;
    sigaction(SIGALRM, &action, NULL);
    struct timespec start, end;
    clock_gettime(CLOCK_MONOTONIC, &start);
    struct itimerval timer = {{0, 0}, {0, 5000}};
    setitimer(ITIMER_REAL, &timer, NULL);
    while (!fired) {
        if (!pure) syscall(SYS_getppid);
    }
    clock_gettime(CLOCK_MONOTONIC, &end);
    long waited = (end.tv_sec - start.tv_sec) * 1000000L + (end.tv_nsec - start.tv_nsec) / 1000;
    printf("self_alarm %s waited_us=%ld\n", pure ? "pure" : "getppid", waited);
    return 0;
}
