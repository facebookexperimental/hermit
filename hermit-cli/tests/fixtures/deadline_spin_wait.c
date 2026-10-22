/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// https://github.com/rrnewton/hermit/issues/4121 variants: main waits 5 ms (nanosleep, a timed condition
// variable wait, poll with no fds, or poll on a pipe nobody writes) while a
// sibling spins (on getppid, or with no syscalls at all) until main is back.
// With "late", the sibling first sleeps 1 ms, so main's wait is already
// registered when the spinning starts.
// Prints how much CLOCK_MONOTONIC time main's wait took, in microseconds.
#include <poll.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static atomic_int done;
static atomic_long iters;
static int pure;
static int late;

static void *spin(void *arg) {
    (void)arg;
    if (late) {
        struct timespec ts = {0, 1000 * 1000};
        nanosleep(&ts, NULL);
    }
    while (!atomic_load(&done)) {
        if (!pure) syscall(SYS_getppid);
        atomic_fetch_add(&iters, 1);
    }
    return NULL;
}

static long now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000L + ts.tv_nsec / 1000;
}

int main(int argc, char **argv) {
    const char *wait = argc > 1 ? argv[1] : "nanosleep";
    pure = argc > 2 && strcmp(argv[2], "pure") == 0;
    late = argc > 3 && strcmp(argv[3], "late") == 0;
    pthread_t t;
    pthread_create(&t, NULL, spin, NULL);
    long start = now_us();
    if (strcmp(wait, "nanosleep") == 0) {
        struct timespec ts = {0, 5 * 1000 * 1000};
        nanosleep(&ts, NULL);
    } else if (strcmp(wait, "condwait") == 0) {
        pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
        pthread_cond_t c;
        pthread_condattr_t a;
        pthread_condattr_init(&a);
        pthread_condattr_setclock(&a, CLOCK_MONOTONIC);
        pthread_cond_init(&c, &a);
        struct timespec ts;
        clock_gettime(CLOCK_MONOTONIC, &ts);
        ts.tv_nsec += 5 * 1000 * 1000;
        if (ts.tv_nsec >= 1000000000L) { ts.tv_sec++; ts.tv_nsec -= 1000000000L; }
        pthread_mutex_lock(&m);
        pthread_cond_timedwait(&c, &m, &ts);
        pthread_mutex_unlock(&m);
    } else if (strcmp(wait, "poll") == 0) {
        poll(NULL, 0, 5);
    } else if (strcmp(wait, "pollpipe") == 0) {
        int fds[2];
        if (pipe(fds) != 0) return 2;
        struct pollfd p = {.fd = fds[0], .events = POLLIN};
        poll(&p, 1, 5);
    } else {
        return 2;
    }
    long waited = now_us() - start;
    atomic_store(&done, 1);
    pthread_join(t, NULL);
    printf("%s/%s%s waited_us=%ld spun=%d\n", wait, pure ? "pure" : "getppid", late ? "/late" : "", waited,
           atomic_load(&iters) > 0);
    return 0;
}
