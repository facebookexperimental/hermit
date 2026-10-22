/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// https://github.com/rrnewton/hermit/issues/4121: a thread's first slice is
// capped too. W starts a 5 ms sleep; only then does main create S, which
// spins on getppid from its first instruction until W is back. S's first
// slice begins at its start grant, while W's deadline is pending. Prints how
// much CLOCK_MONOTONIC time W's sleep took, in microseconds.
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static atomic_int w_ready, done;
static long w_waited_us = -1;

static long now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000L + ts.tv_nsec / 1000;
}

static void *w_fn(void *arg) {
    (void)arg;
    long start = now_us();
    atomic_store(&w_ready, 1);
    struct timespec ts = {0, 5 * 1000 * 1000};
    nanosleep(&ts, NULL);
    w_waited_us = now_us() - start;
    atomic_store(&done, 1);
    return NULL;
}

static void *s_fn(void *arg) {
    (void)arg;
    while (!atomic_load(&done)) syscall(SYS_getppid);
    return NULL;
}

int main(void) {
    pthread_t w, s;
    pthread_create(&w, NULL, w_fn, NULL);
    while (!atomic_load(&w_ready)) syscall(SYS_sched_yield);
    pthread_create(&s, NULL, s_fn, NULL);
    pthread_join(w, NULL);
    pthread_join(s, NULL);
    printf("start_cap w_waited_us=%ld\n", w_waited_us);
    return 0;
}
