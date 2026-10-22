/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved. Licensed under the BSD-style license in LICENSE. */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* A surviving parent checks the real child's wait status. The default-fatal
 * nanosleep also has an inaccessible remainder pointer: fatal delivery must
 * precede any attempt to copy an interrupted sleep's remaining time out. */
enum mode { FATAL_PAUSE, FATAL_SLEEP, IGNORE_SLEEP, BLOCK_SLEEP, CANCEL_SLEEP, RUNNING_ALARM };
static const char *names[] = {
    "default-pause", "default-nanosleep", "ignored-nanosleep",
    "blocked-nanosleep", "cancelled-nanosleep", "running-alarm"
};

static int sample(enum mode mode, const char *phase, int expected_pending) {
    struct sigaction action;
    sigset_t mask, pending;
    if (sigaction(SIGALRM, NULL, &action) ||
        sigprocmask(SIG_SETMASK, NULL, &mask) || sigpending(&pending))
        return 1;
    int disposition = action.sa_handler == SIG_DFL ? 0 :
                      action.sa_handler == SIG_IGN ? 1 : 2;
    int blocked = sigismember(&mask, SIGALRM);
    int waiting = sigismember(&pending, SIGALRM);
    printf("%s action=%d blocked=%d pending=%d\n", phase,
           disposition, blocked, waiting);
    return disposition != (mode == IGNORE_SLEEP) ||
           blocked != (mode == BLOCK_SLEEP) || waiting != expected_pending;
}

static void child(enum mode mode) {
    /* The action and blocked bit were set by the parent before fork; no
     * child-side rewrite can hide incorrect inheritance. The parent's alarm
     * state remains independent of the child's one-shot timer. */
    if (sample(mode, "before", 0)) _exit(90);
    void *inaccessible = NULL;
    if (mode == FATAL_SLEEP) {
        inaccessible = mmap(NULL, 4096, PROT_NONE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (inaccessible == MAP_FAILED) _exit(91);
    }
    if (mode == RUNNING_ALARM) {
        /* getpid costs 250 virtual ns. The bounded loop accrues more than
         * this one-shot millisecond deadline; the next scheduler turn must
         * process expiry outside any emulated sleep. */
        struct itimerval timer = {{0, 0}, {0, 1000}};
        if (setitimer(ITIMER_REAL, &timer, NULL)) _exit(99);
        for (int i = 0; i < 20000; ++i) syscall(SYS_getpid);
        puts("running_alarm_did_not_fire");
        _exit(98);
    }
    unsigned int previous = alarm(1);
    printf("alarm_previous=%u\n", previous);
    if (previous != 0) _exit(92);
    if (mode == CANCEL_SLEEP) {
        unsigned int remaining = alarm(0);
        printf("cancel_before_sleep=%u\n", remaining);
        if (remaining != 1) _exit(93);
    }
    errno = 0;
    if (mode == FATAL_PAUSE) {
        long result = syscall(SYS_pause);
        printf("unexpected_pause=%ld errno=%d\n", result, errno);
        _exit(94);
    }
    struct timespec duration = {2, 0};
    long result = syscall(SYS_nanosleep, &duration, inaccessible);
    printf("nanosleep=%ld errno=%d\n", result, errno);
    if (mode == FATAL_SLEEP || result != 0 || errno != 0) _exit(95);
    int pending = mode == BLOCK_SLEEP;
    if (sample(mode, "after_sleep", pending)) _exit(96);
    unsigned int remaining = alarm(0);
    printf("cancel_after_sleep=%u\n", remaining);
    if (remaining != 0 || sample(mode, "after_cancel", pending)) _exit(97);
    _exit(0);
}

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    enum mode mode;
    for (mode = FATAL_PAUSE; mode <= RUNNING_ALARM; ++mode)
        if (strcmp(argv[1], names[mode]) == 0) break;
    if (mode > RUNNING_ALARM) return 2;
    setvbuf(stdout, NULL, _IONBF, 0);
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = mode == IGNORE_SLEEP ? SIG_IGN : SIG_DFL;
    sigemptyset(&action.sa_mask);
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGALRM);
    if (sigaction(SIGALRM, &action, NULL) ||
        sigprocmask(mode == BLOCK_SLEEP ? SIG_BLOCK : SIG_UNBLOCK, &mask, NULL))
        return 3;
    pid_t pid = fork();
    if (pid < 0) return 4;
    if (pid == 0) child(mode);
    int status;
    pid_t waited;
    do { waited = waitpid(pid, &status, 0); } while (waited < 0 && errno == EINTR);
    if (waited != pid) return 5;
    int signal_number = WIFSIGNALED(status) ? WTERMSIG(status) : 0;
    int code = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
    int core = WIFSIGNALED(status) && WCOREDUMP(status) ? 1 : 0;
    printf("child_signal=%d child_exit=%d core=%d\n", signal_number, code, core);
    unsigned int parent_alarm = alarm(0);
    printf("parent_alarm=%u\n", parent_alarm);
    int fatal = mode == FATAL_PAUSE || mode == FATAL_SLEEP || mode == RUNNING_ALARM;
    return parent_alarm != 0 || core != 0 ||
           (fatal ? signal_number != SIGALRM || code != -1 : signal_number != 0 || code != 0);
}
