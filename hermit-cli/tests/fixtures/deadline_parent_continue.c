/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// https://github.com/rrnewton/hermit/issues/4121: the parent's continuation
// after clone is a grant too. The new thread W runs first and starts a 5 ms
// sleep; the parent, resumed from clone while W's deadline is pending, spins
// on getppid until W is back. Prints how much CLOCK_MONOTONIC time W's sleep
// took, in microseconds.
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static atomic_int done;
static long w_waited_us = -1;

static long now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000L + ts.tv_nsec / 1000;
}

static void *w_fn(void *arg) {
    (void)arg;
    long start = now_us();
    struct timespec ts = {0, 5 * 1000 * 1000};
    nanosleep(&ts, NULL);
    w_waited_us = now_us() - start;
    atomic_store(&done, 1);
    return NULL;
}

int main(void) {
    pthread_t w;
    pthread_create(&w, NULL, w_fn, NULL);
    while (!atomic_load(&done)) syscall(SYS_getppid);
    pthread_join(w, NULL);
    printf("parent_continue w_waited_us=%ld\n", w_waited_us);
    return 0;
}
