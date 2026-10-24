/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved. Licensed under the BSD-style license in LICENSE. */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Raw queries avoid libc identity caches. The caller compares every printed
 * value and errno, including those sampled while SIGALRM runs its handler. */
static const long calls[4] = {SYS_getuid, SYS_geteuid, SYS_getgid, SYS_getegid};
static const char *names[4] = {"getuid", "geteuid", "getgid", "getegid"};
static const char *stages[5] = {
    "before", "installed", "handler", "after_delivery", "removed"
};
static long values[5][4];
static int errors[5][4];
static volatile sig_atomic_t hits, last_signal;
static volatile sig_atomic_t inside_value[4], inside_errno[4];

static void sample(int stage) {
    for (int i = 0; i < 4; i++) {
        errno = 0;
        values[stage][i] = syscall(calls[i], 0L, 0L, 0L, 0L, 0L, 0L);
        errors[stage][i] = errno;
    }
}

static void handler(int signo) {
    int saved_errno = errno;
    hits++;
    last_signal = signo;
    for (int i = 0; i < 4; i++) {
        errno = 0;
        inside_value[i] = (sig_atomic_t)syscall(calls[i], 0L, 0L, 0L, 0L, 0L, 0L);
        inside_errno[i] = errno;
    }
    errno = saved_errno;
}

static int show_sample(int stage, const char *label) {
    int failed = 0;
    for (int i = 0; i < 4; i++) {
        printf("%s %s value=%ld errno=%d\n", label, names[i],
               values[stage][i], errors[stage][i]);
        failed |= values[stage][i] != 0 || errors[stage][i] != 0;
    }
    return failed;
}

int main(int argc, char **argv) {
    if (argc != 2 || (strcmp(argv[1], "handled") != 0 &&
                      strcmp(argv[1], "refused") != 0))
        return 2;
    int expect_refusal = strcmp(argv[1], "refused") == 0;
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = handler;
    sigemptyset(&action.sa_mask);
    sample(0);
    errno = 0;
    int install = sigaction(SIGALRM, &action, NULL);
    int install_errno = errno;
    if (expect_refusal || install != 0) {
        sample(4);
        printf("install=%d errno=%d hits=%d\n", install, install_errno, (int)hits);
        int failed = show_sample(0, "before");
        failed |= show_sample(4, "after_refusal");
        return failed || !expect_refusal || install != -1 || install_errno != EPERM || hits != 0;
    }
    sample(1);
    errno = 0;
    long alarm_result = syscall(SYS_alarm, 1L);
    int alarm_errno = errno;
    errno = 0;
    long pause_result = syscall(SYS_pause);
    int pause_errno = errno;
    for (int i = 0; i < 4; i++) {
        values[2][i] = inside_value[i];
        errors[2][i] = inside_errno[i];
    }
    sample(3);
    /* The one-shot timer has fired. Ignoring removes the virtual handler;
     * this fixture does not exercise a pending default-fatal transition. */
    action.sa_handler = SIG_IGN;
    errno = 0;
    int removed = sigaction(SIGALRM, &action, NULL);
    int remove_errno = errno;
    sample(4);
    printf("install=%d errno=%d alarm=%ld errno=%d pause=%ld errno=%d "
           "hits=%d signal=%d remove=%d errno=%d\n",
           install, install_errno, alarm_result, alarm_errno, pause_result,
           pause_errno, (int)hits, (int)last_signal, removed, remove_errno);
    int failed = 0;
    for (int stage = 0; stage < 5; stage++)
        failed |= show_sample(stage, stages[stage]);
    return failed || install_errno != 0 || alarm_result != 0 || alarm_errno != 0 ||
           pause_result != -1 || pause_errno != EINTR || hits != 1 ||
           last_signal != SIGALRM || removed != 0 || remove_errno != 0;
}
