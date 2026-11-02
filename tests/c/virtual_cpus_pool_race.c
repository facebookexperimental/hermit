/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A worker pool sized from sysconf(_SC_NPROCESSORS_ONLN), as TBB, folly and
// std::thread::hardware_concurrency users size theirs. Each worker adds 1 to
// a shared counter many times with an unsynchronized read and write, and
// gives up the CPU in between, as a worker that logs or takes another lock
// there does. With one worker nothing races and no update is lost. With two
// or more, another worker can run between a read and its write, and the
// write overwrites its updates. Prints the pool's size and whether any
// update was lost.
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#define ITERATIONS 200

static volatile long counter;

static void *work(void *unused) {
    (void)unused;
    for (int i = 0; i < ITERATIONS; i++) {
        long seen = counter;
        sched_yield();
        counter = seen + 1;
    }
    return NULL;
}

int main(void) {
    long workers = sysconf(_SC_NPROCESSORS_ONLN);
    pthread_t *pool = calloc((size_t)workers, sizeof(pthread_t));
    if (workers < 1 || pool == NULL) {
        return 2;
    }
    for (long i = 0; i < workers; i++) {
        if (pthread_create(&pool[i], NULL, work, NULL) != 0) {
            return 2;
        }
    }
    for (long i = 0; i < workers; i++) {
        pthread_join(pool[i], NULL);
    }
    printf("workers=%ld %s\n", workers,
           counter == workers * ITERATIONS ? "no update lost" : "updates lost");
    free(pool);
    return 0;
}
