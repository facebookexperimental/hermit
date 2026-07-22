/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * NONDET_SOURCE: Unsynchronized counter updates race, and native scheduling
 * changes both the lost-update count and thread completion order.
 */

#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#define THREAD_COUNT 8
#define INCREMENTS_PER_THREAD 250000

static volatile uint64_t shared_counter;
static pthread_barrier_t start_barrier;
static unsigned int completion_order[THREAD_COUNT];
static _Atomic unsigned int completion_count;

static void fail_pthread(const char *operation, int error) {
    fprintf(stderr, "%s failed: %d\n", operation, error);
    exit(EXIT_FAILURE);
}

static void *increment_counter(void *argument) {
    uintptr_t id = (uintptr_t)argument;
    int error = pthread_barrier_wait(&start_barrier);
    if (error != 0 && error != PTHREAD_BARRIER_SERIAL_THREAD) {
        fail_pthread("pthread_barrier_wait", error);
    }

    /*
     * Volatile keeps the compiler from collapsing the loop, but deliberately
     * provides no atomicity or synchronization between worker threads.
     */
    for (unsigned int i = 0; i < INCREMENTS_PER_THREAD; ++i) {
        shared_counter++;
    }

    /*
     * Record completion through a separate atomic index so observing the race
     * does not introduce another data race into the diagnostic output.
     */
    unsigned int slot =
        atomic_fetch_add_explicit(&completion_count, 1, memory_order_relaxed);
    completion_order[slot] = (unsigned int)id;
    return NULL;
}

int main(void) {
    pthread_t threads[THREAD_COUNT];
    int error =
        pthread_barrier_init(&start_barrier, NULL, THREAD_COUNT + 1);
    if (error != 0) {
        fail_pthread("pthread_barrier_init", error);
    }

    for (uintptr_t id = 0; id < THREAD_COUNT; ++id) {
        error =
            pthread_create(&threads[id], NULL, increment_counter, (void *)id);
        if (error != 0) {
            fail_pthread("pthread_create", error);
        }
    }

    error = pthread_barrier_wait(&start_barrier);
    if (error != 0 && error != PTHREAD_BARRIER_SERIAL_THREAD) {
        fail_pthread("pthread_barrier_wait", error);
    }

    for (unsigned int id = 0; id < THREAD_COUNT; ++id) {
        error = pthread_join(threads[id], NULL);
        if (error != 0) {
            fail_pthread("pthread_join", error);
        }
    }

    error = pthread_barrier_destroy(&start_barrier);
    if (error != 0) {
        fail_pthread("pthread_barrier_destroy", error);
    }

    printf("counter=%llu expected=%u order=",
           (unsigned long long)shared_counter,
           THREAD_COUNT * INCREMENTS_PER_THREAD);
    for (unsigned int i = 0; i < THREAD_COUNT; ++i) {
        printf("%s%u", i == 0 ? "" : ",", completion_order[i]);
    }
    putchar('\n');
    return 0;
}
