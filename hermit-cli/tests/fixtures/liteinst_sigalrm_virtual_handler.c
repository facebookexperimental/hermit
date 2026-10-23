/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Signal phase 1, step I3 (in-guest LiteInst): a guest SIGALRM handler is
 * installed virtually, Detcore learns of it, the scheduler's ledger holds an
 * expiry while SIGALRM is blocked, and leaving the handler restores the
 * ordinary path. Prints one line per observation; the test compares them.
 *
 * - install: the handler's rt_sigaction result and errno;
 * - query_is_handler: whether a query returns the handler (the virtual action);
 * - sigpending: phase 1's table refuses rt_sigpending while SIGALRM is handled
 *   (EOPNOTSUPP, 95); without a handler it succeeds;
 * - socket and fork: two more calls the table refuses while SIGALRM is
 *   handled, so Detcore knows the handler is installed;
 * - to_default: with SIGALRM blocked, a one-shot ITIMER_REAL expires while
 *   the guest writes; the expiry is pending in the ledger, so a change to
 *   SIG_DFL is refused (EPERM, 1). Without a handler the expiry is a physical
 *   SIGALRM, blocked, and the change is accepted;
 * - ignore and socket_after: SIG_IGN discards the pending SIGALRM, and the
 *   socket then succeeds.
 */

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

static void handler(int signal) { (void)signal; }

static void report_socket(const char *label) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    printf("%s=%s errno=%d\n", label, fd < 0 ? "refused" : "ok", fd < 0 ? errno : 0);
    if (fd >= 0) {
        close(fd);
    }
}

int main(void) {
    struct sigaction action;
    struct sigaction queried;
    memset(&action, 0, sizeof action);
    memset(&queried, 0, sizeof queried);
    action.sa_handler = handler;
    int result = sigaction(SIGALRM, &action, NULL);
    printf("install=%d errno=%d\n", result, result != 0 ? errno : 0);
    sigaction(SIGALRM, NULL, &queried);
    printf("query_is_handler=%d\n", queried.sa_handler == handler);
    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGALRM);
    sigprocmask(SIG_BLOCK, &blocked, NULL);
    sigset_t pending;
    sigemptyset(&pending);
    result = sigpending(&pending);
    printf("sigpending=%d errno=%d\n", result, result != 0 ? errno : 0);
    report_socket("socket");
    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        _exit(0);
    }
    if (child < 0) {
        printf("fork=refused errno=%d\n", errno);
    } else {
        int status = 0;
        int reaped = waitpid(child, &status, 0) == child && WIFEXITED(status);
        printf("fork=%s errno=0\n", reaped ? "ok" : "unreaped");
    }
    struct itimerval timer;
    memset(&timer, 0, sizeof timer);
    timer.it_value.tv_usec = 1;
    if (setitimer(ITIMER_REAL, &timer, NULL) != 0) {
        printf("setitimer_failed errno=%d\n", errno);
        return 1;
    }
    /* Scheduler turns, during which the expiry commits. */
    for (int i = 0; i < 200; i++) {
        if (write(STDERR_FILENO, ".", 1) != 1) {
            return 1;
        }
    }
    action.sa_handler = SIG_DFL;
    result = sigaction(SIGALRM, &action, NULL);
    printf("to_default=%d errno=%d\n", result, result != 0 ? errno : 0);
    action.sa_handler = SIG_IGN;
    printf("ignore=%d\n", sigaction(SIGALRM, &action, NULL));
    report_socket("socket_after");
    return 0;
}
