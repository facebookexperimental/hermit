/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A chaos guest whose observable outcome is the thread INTERLEAVING, not a
 * boolean race verdict.
 *
 * tests/chaos/order_violation.c has exactly two observable outcomes, so the
 * `distinct` half of the chaos diversity oracle is pinned at its ceiling: the
 * check `distinct >= 2` is simultaneously the floor and the maximum, and it
 * therefore detects only a TOTAL collapse of schedule diversity, never a
 * partial narrowing. This guest exists to give that oracle headroom.
 *
 * Each of THREADS workers appends its own id to a shared buffer, guarded so
 * the buffer itself never races (a data race would be nondeterminism, which is
 * exactly what the surrounding --strict run must NOT have). What varies is the
 * ORDER in which the scheduler lets the workers reach their append, so the
 * printed sequence is a permutation of 0..THREADS-1. The observation is that
 * permutation, giving up to THREADS! distinct outcome classes -- 24 at
 * THREADS=4 -- against which a drop in schedule diversity is measurable as a
 * drop in the observed class count rather than being invisible.
 *
 * Determinism contract: for a FIXED chaos seed this program is fully
 * deterministic under Hermit. Every worker's observable effect is a single
 * mutex-protected append, so the printed permutation is a pure function of the
 * scheduling decisions Detcore makes, and those are themselves a function of
 * the seed. Nothing here reads wall-clock time, host randomness, pids, or
 * addresses. The harness enforces this: it runs every seed twice and requires
 * both runs to agree bit-for-bit (`repeat_mismatches` must be 0).
 */

#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

#define THREADS 4

static pthread_mutex_t order_lock = PTHREAD_MUTEX_INITIALIZER;
static int order[THREADS];
static int order_len = 0;

static void* worker(void* arg) {
    int id = (int)(long)arg;

    /*
     * A short bounded spin before the append. It carries no data dependence
     * and no observable effect of its own; it exists only to widen the window
     * in which the scheduler may preempt this thread, so that which worker
     * arrives next is decided by scheduling rather than by thread-creation
     * order. The bound is a compile-time constant, so the work performed is
     * identical on every run.
     */
    volatile int sink = 0;
    for (int i = 0; i < 20000; i++) {
        sink += i;
    }
    (void)sink;

    pthread_mutex_lock(&order_lock);
    order[order_len++] = id;
    pthread_mutex_unlock(&order_lock);
    return NULL;
}

int main(void) {
    pthread_t threads[THREADS];

    for (long i = 0; i < THREADS; i++) {
        if (pthread_create(&threads[i], NULL, worker, (void*)i) != 0) {
            fprintf(stderr, "pthread_create failed\n");
            return 2;
        }
    }
    for (int i = 0; i < THREADS; i++) {
        pthread_join(threads[i], NULL);
    }

    if (order_len != THREADS) {
        fprintf(stderr, "ERROR! recorded %d of %d appends\n", order_len, THREADS);
        return 2;
    }

    /*
     * The permutation IS the observation. Print it on one line so the harness's
     * stdout hash is exactly the interleaving class.
     */
    for (int i = 0; i < THREADS; i++) {
        printf("%d%s", order[i], (i + 1 == THREADS) ? "\n" : " ");
    }
    return 0;
}
