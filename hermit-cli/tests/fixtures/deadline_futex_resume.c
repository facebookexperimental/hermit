/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// https://github.com/rrnewton/hermit/issues/4121: a futex wake is a grant too. A waits on a futex with no timeout. B sleeps 5 ms.
// C wakes A, then exits. A, woken by a real futex grant, spins (pure loop, or
// getppid) until B is back. With the cap carried by A's futex reply, B's
// sleep ends near 5 ms; without it, B waits A's whole slice.
// argv[1]: "pure" or "getppid"; argv[2]: "sleep" (B sleeps) or "nosleep"
// (no deadline anywhere: control).
#include <linux/futex.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static atomic_int word, done, a_waiting;
static int pure, sleeping;
static long b_waited_us = -1;

static long now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000L + ts.tv_nsec / 1000;
}

static void *a_fn(void *arg) {
    (void)arg;
    atomic_store(&a_waiting, 1);
    while (atomic_load(&word) == 0)
        syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
    long n = 0;
    while (!atomic_load(&done)) {
        if (!pure) syscall(SYS_getppid);
        if (!sleeping && ++n == 2000000) break;
    }
    return NULL;
}

static void *b_fn(void *arg) {
    (void)arg;
    if (sleeping) {
        long t0 = now_us();
        struct timespec ts = {0, 5 * 1000 * 1000};
        nanosleep(&ts, NULL);
        b_waited_us = now_us() - t0;
    }
    atomic_store(&done, 1);
    return NULL;
}

static void *c_fn(void *arg) {
    (void)arg;
    while (!atomic_load(&a_waiting)) syscall(SYS_sched_yield);
    atomic_store(&word, 1);
    syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
    return NULL;
}

int main(int argc, char **argv) {
    pure = argc > 1 && strcmp(argv[1], "pure") == 0;
    sleeping = !(argc > 2 && strcmp(argv[2], "nosleep") == 0);
    pthread_t a, b, c;
    pthread_create(&a, NULL, a_fn, NULL);
    pthread_create(&b, NULL, b_fn, NULL);
    pthread_create(&c, NULL, c_fn, NULL);
    pthread_join(c, NULL);
    pthread_join(b, NULL);
    pthread_join(a, NULL);
    printf("futex_resume %s/%s b_waited_us=%ld\n", pure ? "pure" : "getppid",
           sleeping ? "sleep" : "nosleep", b_waited_us);
    return 0;
}
