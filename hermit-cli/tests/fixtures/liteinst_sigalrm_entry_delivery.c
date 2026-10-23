/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Signal phase 1, the I4 addendum (in-guest LiteInst): a SIGALRM that
 * becomes pending while SIGALRM is blocked inside a handler is delivered as
 * soon as that handler returns, before the guest's next syscall, as Linux
 * delivers it at the handler's rt_sigreturn. Every line is one write(2) to
 * stdout, so the order is the order the calls ran in.
 *
 * Handlers 1, 3 and 5 arm a one-shot 1-microsecond ITIMER_REAL and make 200
 * writes to stderr, during which it expires while SIGALRM is blocked (no
 * nesting: "end" precedes the next handler). Handlers 2, 4 and 6 query the
 * mask (SIGALRM is blocked inside a handler) and print; the guest's next
 * alarm and pause after such a handler still work.
 * - after handler 1, the next syscall is the write of "after pause":
 *   handler 2 runs first;
 * - handler 3 also adds SIGALRM to its saved mask: nothing runs at its
 *   return, the guest sees SIGALRM blocked, and handler 4 runs when
 *   sigprocmask unblocks it, before that call returns;
 * - after handler 5, the next syscall is exit_group: handler 6 runs first.
 */

#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <ucontext.h>
#include <unistd.h>

static volatile sig_atomic_t runs;

static void say(const char *text) {
    if (write(STDOUT_FILENO, text, strlen(text)) < 0) {
        _exit(2);
    }
}

static void handler(int signal, siginfo_t *info, void *context) {
    (void)signal;
    (void)info;
    char line[64];
    int run = ++runs;
    if (run % 2 == 0) {
        sigset_t inside;
        sigprocmask(SIG_BLOCK, NULL, &inside);
        snprintf(line, sizeof line, "handler %d blocked=%d\n", run, sigismember(&inside, SIGALRM));
        say(line);
        return;
    }
    snprintf(line, sizeof line, "handler %d start\n", run);
    say(line);
    struct itimerval timer;
    memset(&timer, 0, sizeof timer);
    timer.it_value.tv_usec = 1;
    if (setitimer(ITIMER_REAL, &timer, NULL) != 0) {
        _exit(3);
    }
    for (int i = 0; i < 200; i++) {
        if (write(STDERR_FILENO, ".", 1) != 1) {
            _exit(4);
        }
    }
    if (run == 3) {
        sigaddset(&((ucontext_t *)context)->uc_sigmask, SIGALRM);
    }
    snprintf(line, sizeof line, "handler %d end\n", run);
    say(line);
}

int main(void) {
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_sigaction = handler;
    action.sa_flags = SA_SIGINFO;
    if (sigaction(SIGALRM, &action, NULL) != 0) {
        say("install failed\n");
        return 1;
    }

    alarm(1);
    pause();
    say("after pause\n");

    alarm(1);
    pause();
    sigset_t current;
    sigprocmask(SIG_BLOCK, NULL, &current);
    say(sigismember(&current, SIGALRM) ? "blocked after return\n" : "unblocked after return\n");
    sigset_t alarm_only;
    sigemptyset(&alarm_only);
    sigaddset(&alarm_only, SIGALRM);
    sigprocmask(SIG_UNBLOCK, &alarm_only, NULL);
    say("after unblock\n");

    alarm(1);
    pause();
    syscall(SYS_exit_group, 0);
    return 1;
}
