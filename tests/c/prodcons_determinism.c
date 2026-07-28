/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Bounded-queue producer/consumer determinism guest. Multiple producers and
// consumers share a fixed-capacity ring guarded by a mutex and two condition
// variables (not-full / not-empty). A fixed amount of work flows through the
// queue and each consumed payload is folded into an order-independent checksum.
// Hermit must serialize the mutex/condvar wakeups deterministically, so the
// printed counts and checksum are identical across runs. This is distinct from
// a spin-contention probe: the threads block on condition variables rather than
// busy-waiting. Prints "prodcons success" as the final line on success.
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>

#define CAP 8
#define NPROD 4
#define NCONS 3
#define PER_PROD 500
#define TOTAL (NPROD * PER_PROD)

static int buf[CAP];
static int head, tail, count;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t not_full = PTHREAD_COND_INITIALIZER;
static pthread_cond_t not_empty = PTHREAD_COND_INITIALIZER;

static int produced_total, consumed_total;
static uint64_t consumed_sum;

static void *producer(void *arg) {
    long id = (long)arg;
    for (int i = 0; i < PER_PROD; i++) {
        int item = (int)(id * 100000 + i);
        pthread_mutex_lock(&mu);
        while (count == CAP) {
            pthread_cond_wait(&not_full, &mu);
        }
        buf[tail] = item;
        tail = (tail + 1) % CAP;
        count++;
        produced_total++;
        pthread_cond_signal(&not_empty);
        pthread_mutex_unlock(&mu);
    }
    return NULL;
}

static void *consumer(void *arg) {
    (void)arg;
    for (;;) {
        pthread_mutex_lock(&mu);
        while (count == 0 && consumed_total < TOTAL) {
            pthread_cond_wait(&not_empty, &mu);
        }
        if (count == 0 && consumed_total >= TOTAL) {
            pthread_mutex_unlock(&mu);
            break;
        }
        int item = buf[head];
        head = (head + 1) % CAP;
        count--;
        consumed_total++;
        consumed_sum += (uint64_t)(uint32_t)item;
        pthread_cond_signal(&not_full);
        pthread_cond_broadcast(&not_empty);
        pthread_mutex_unlock(&mu);
    }
    return NULL;
}

int main(void) {
    pthread_t p[NPROD], c[NCONS];
    for (long i = 0; i < NPROD; i++) {
        pthread_create(&p[i], NULL, producer, (void *)i);
    }
    for (long i = 0; i < NCONS; i++) {
        pthread_create(&c[i], NULL, consumer, (void *)i);
    }
    for (int i = 0; i < NPROD; i++) {
        pthread_join(p[i], NULL);
    }
    pthread_mutex_lock(&mu);
    pthread_cond_broadcast(&not_empty);
    pthread_mutex_unlock(&mu);
    for (int i = 0; i < NCONS; i++) {
        pthread_join(c[i], NULL);
    }

    uint64_t expect = 0;
    for (long id = 0; id < NPROD; id++) {
        for (int i = 0; i < PER_PROD; i++) {
            expect += (uint64_t)(uint32_t)(int)(id * 100000 + i);
        }
    }

    printf("produced=%d\n", produced_total);
    printf("consumed=%d\n", consumed_total);
    printf("consumed_sum=%llu\n", (unsigned long long)consumed_sum);
    if (produced_total != TOTAL || consumed_total != TOTAL || consumed_sum != expect) {
        printf("prodcons MISMATCH expect_sum=%llu\n", (unsigned long long)expect);
        return 1;
    }
    printf("prodcons success\n");
    return 0;
}
