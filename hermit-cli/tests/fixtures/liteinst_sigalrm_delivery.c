/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Signal phase 1, step I4 (in-guest LiteInst): a guest SIGALRM handler runs.
 * Prints one line per observation; the test compares them.
 *
 * - pause: alarm(1) then pause(); the handler runs once and pause returns
 *   EINTR (4). The handler sees SIGALRM, si_code SI_KERNEL (128), SIGALRM
 *   blocked inside it, and the mask is restored after it returns;
 * - unblock: with SIGALRM blocked, a one-shot ITIMER_REAL expires while the
 *   guest writes; nothing runs until sigprocmask unblocks it, and the handler
 *   has run once when that call returns.
 *
 * No argument and "completion" preserve those observations. The return modes
 * observe the real SA_SIGINFO frame and activation mask, leave uc_sigmask
 * unchanged or add SIGUSR1, then query the mask after the handler returns.
 * SIGUSR1 is in sa_mask but is never sent.
 */

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <ucontext.h>
#include <unistd.h>

static volatile sig_atomic_t runs;
static volatile sig_atomic_t last_signal;
static volatile sig_atomic_t last_code;
static volatile sig_atomic_t blocked_inside;

static void handler(int signal, siginfo_t *info, void *context) {
    (void)context;
    sigset_t current;
    sigprocmask(SIG_BLOCK, NULL, &current);
    runs++;
    last_signal = signal;
    last_code = info->si_code;
    blocked_inside = sigismember(&current, SIGALRM);
}

static int alarm_blocked(void) {
    sigset_t current;
    sigprocmask(SIG_BLOCK, NULL, &current);
    return sigismember(&current, SIGALRM);
}

static int completion(void) {
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_sigaction = handler;
    action.sa_flags = SA_SIGINFO;
    int result = sigaction(SIGALRM, &action, NULL);
    printf("install=%d errno=%d\n", result, result != 0 ? errno : 0);
    fflush(stdout);

    alarm(1);
    result = pause();
    printf("pause=%d errno=%d runs=%d signal=%d code=%d blocked_inside=%d blocked_after=%d\n",
           result, result != 0 ? errno : 0, (int)runs, (int)last_signal, (int)last_code,
           (int)blocked_inside, alarm_blocked());
    fflush(stdout);

    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGALRM);
    sigprocmask(SIG_BLOCK, &blocked, NULL);
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
    int before = runs;
    sigprocmask(SIG_UNBLOCK, &blocked, NULL);
    int after = runs;
    printf("unblock: before=%d after=%d blocked_after=%d\n", before - 1, after - 1, alarm_blocked());
    return 0;
}

static volatile sig_atomic_t edit_usr1;
static volatile sig_atomic_t saved_alrm;
static volatile sig_atomic_t saved_usr1;
static volatile sig_atomic_t active_alrm;
static volatile sig_atomic_t active_usr1;
static volatile sig_atomic_t return_alrm;
static volatile sig_atomic_t return_usr1;

static void return_failure(void) {
    static const char message[] = "signal return observation failed\n";
    (void)write(STDERR_FILENO, message, sizeof message - 1);
    _exit(2);
}

static int mask_member(const sigset_t *mask, int signal) {
    int member = sigismember(mask, signal);
    if (member < 0) {
        return_failure();
    }
    return member;
}

static void return_handler(int signal, siginfo_t *info, void *context) {
    if (info == NULL || context == NULL) {
        return_failure();
    }
    ucontext_t *frame = context;
    saved_alrm = mask_member(&frame->uc_sigmask, SIGALRM);
    saved_usr1 = mask_member(&frame->uc_sigmask, SIGUSR1);

    sigset_t current;
    if (sigprocmask(SIG_BLOCK, NULL, &current) != 0) {
        return_failure();
    }
    active_alrm = mask_member(&current, SIGALRM);
    active_usr1 = mask_member(&current, SIGUSR1);
    if (edit_usr1 && sigaddset(&frame->uc_sigmask, SIGUSR1) != 0) {
        return_failure();
    }
    return_alrm = mask_member(&frame->uc_sigmask, SIGALRM);
    return_usr1 = mask_member(&frame->uc_sigmask, SIGUSR1);
    runs++;
    last_signal = signal;
    last_code = info->si_code;
}

static int observe_return(int edit) {
    sigset_t original;
    sigset_t unblocked;
    sigset_t before;
    if (sigprocmask(SIG_BLOCK, NULL, &original) != 0 ||
        sigemptyset(&unblocked) != 0 || sigaddset(&unblocked, SIGALRM) != 0 ||
        sigaddset(&unblocked, SIGUSR1) != 0 ||
        sigprocmask(SIG_UNBLOCK, &unblocked, NULL) != 0 ||
        sigprocmask(SIG_BLOCK, NULL, &before) != 0) {
        return_failure();
    }

    edit_usr1 = edit;
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_sigaction = return_handler;
    action.sa_flags = SA_SIGINFO;
    if (sigemptyset(&action.sa_mask) != 0 || sigaddset(&action.sa_mask, SIGUSR1) != 0 ||
        sigaction(SIGALRM, &action, NULL) != 0) {
        return_failure();
    }
    if (printf("install=0 errno=0 before_alrm=%d before_usr1=%d\n",
               mask_member(&before, SIGALRM), mask_member(&before, SIGUSR1)) < 0 ||
        fflush(stdout) != 0) {
        return_failure();
    }

    alarm(1);
    int result = pause();
    int pause_errno = result != 0 ? errno : 0;
    sigset_t after;
    if (sigprocmask(SIG_BLOCK, NULL, &after) != 0) {
        return_failure();
    }
    int after_alrm = mask_member(&after, SIGALRM);
    int after_usr1 = mask_member(&after, SIGUSR1);
    if (sigprocmask(SIG_SETMASK, &original, NULL) != 0) {
        return_failure();
    }
    if (printf("pause=%d errno=%d runs=%d signal=%d code=%d\n"
               "saved_alrm=%d saved_usr1=%d active_alrm=%d active_usr1=%d\n"
               "return_alrm=%d return_usr1=%d\n"
               "after_alrm=%d after_usr1=%d\n",
               result, pause_errno, (int)runs, (int)last_signal, (int)last_code,
               (int)saved_alrm, (int)saved_usr1, (int)active_alrm, (int)active_usr1,
               (int)return_alrm, (int)return_usr1, after_alrm, after_usr1) < 0 ||
        fflush(stdout) != 0) {
        return_failure();
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 1 || (argc == 2 && strcmp(argv[1], "completion") == 0)) {
        return completion();
    }
    if (argc == 2 && strcmp(argv[1], "return-unchanged") == 0) {
        return observe_return(0);
    }
    if (argc == 2 && strcmp(argv[1], "return-edit-usr1") == 0) {
        return observe_return(1);
    }
    return_failure();
}
