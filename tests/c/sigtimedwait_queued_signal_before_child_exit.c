/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* The parent catches SIGCHLD and waits in rt_sigtimedwait for a blocked
 * SIGUSR1. Its child sends SIGUSR1, then calls exit_group. Linux's
 * do_sigtimedwait dequeues the queued SIGUSR1 before it considers EINTR for the
 * child's SIGCHLD, so the call returns SIGUSR1 with the child's siginfo
 * (Codex review of https://github.com/rrnewton/hermit/pull/4039, witness kept
 * as written there). The guest exits 42 on any other result.
 */
static volatile sig_atomic_t chld_calls;
static void on_chld(int signo) {
    if (signo == SIGCHLD) ++chld_calls;
}
static int fail_setup(const char *what) {
    perror(what);
    return 90;
}
int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = on_chld;
    if (sigemptyset(&sa.sa_mask) != 0) return fail_setup("sigemptyset(action)");
    if (sigaction(SIGCHLD, &sa, NULL) != 0) return fail_setup("sigaction(CHLD)");

    sigset_t wanted, chld;
    if (sigemptyset(&wanted) != 0 || sigaddset(&wanted, SIGUSR1) != 0)
        return fail_setup("wait set");
    if (sigprocmask(SIG_BLOCK, &wanted, NULL) != 0)
        return fail_setup("block USR1");
    if (sigemptyset(&chld) != 0 || sigaddset(&chld, SIGCHLD) != 0)
        return fail_setup("CHLD set");
    if (sigprocmask(SIG_UNBLOCK, &chld, NULL) != 0)
        return fail_setup("unblock CHLD");

    pid_t parent = getpid();
    pid_t child = fork();
    if (child < 0) return fail_setup("fork");
    if (child == 0) {
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 100000000};
        unsigned int interrupted = 0;
        while (nanosleep(&pause, &pause) != 0) {
            if (errno != EINTR || ++interrupted > 8) _exit(91);
        }
        if (kill(parent, SIGUSR1) != 0) _exit(92);
        syscall(SYS_exit_group, 7);
        _exit(93);
    }

    struct timespec timeout = {.tv_sec = 1, .tv_nsec = 0};
    siginfo_t info;
    memset(&info, 0, sizeof(info));
    errno = 0;
    long result = syscall(SYS_rt_sigtimedwait, &wanted, &info, &timeout,
                          sizeof(uint64_t));
    int wait_errno = errno;
    int before_reap = chld_calls;
    int status = 0;
    pid_t reaped;
    unsigned int interrupted = 0;
    do {
        reaped = waitpid(child, &status, 0);
    } while (reaped < 0 && errno == EINTR && ++interrupted <= 8);
    int child_ok = reaped == child && WIFEXITED(status) && WEXITSTATUS(status) == 7;
    int signal_ok = result == SIGUSR1 && wait_errno == 0 &&
                    info.si_signo == SIGUSR1 && info.si_code == SI_USER &&
                    info.si_pid == child;
    printf("wait_result=%ld wait_errno=%d expected=%d info_signo=%d info_code=%d "
           "info_from_child=%d chld_before_reap=%d chld_after_reap=%d child_ok=%d\n",
           result, wait_errno, SIGUSR1, info.si_signo, info.si_code,
           info.si_pid == child, before_reap, (int)chld_calls, child_ok);
    return signal_ok && child_ok ? 0 : 42;
}
