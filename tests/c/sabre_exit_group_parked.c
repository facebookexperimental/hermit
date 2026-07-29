/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <assert.h>
#include <linux/futex.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <unistd.h>

static atomic_bool child_started;
static atomic_bool child_reparking;
static uint32_t futex_word;
static uint32_t parked_word;

static void *park_child(void *unused)
{
    (void)unused;
    atomic_store_explicit(&child_started, true, memory_order_release);

    /* The parent wakes this first wait only after confirming that the child parked. */
    (void)syscall(SYS_futex, &futex_word,
                  FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0, NULL, NULL, 0);
    atomic_store_explicit(&child_reparking, true, memory_order_release);

    /* The parent confirms the first wait, then deliberately never wakes this one. */
    (void)syscall(SYS_futex, &parked_word,
                  FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0, NULL, NULL, 0);
    _exit(91);
}

int main(void)
{
    pthread_t child;

    assert(pthread_create(&child, NULL, park_child, NULL) == 0);
    while (!atomic_load_explicit(&child_started, memory_order_acquire)) {
        assert(syscall(SYS_sched_yield) == 0);
    }

    /* Wake one confirmed waiter, then give the child turns to park a second time. */
    for (;;) {
        long woken = syscall(SYS_futex, &futex_word,
                             FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, NULL, NULL, 0);
        assert(woken == 0 || woken == 1);
        if (woken == 1) {
            break;
        }
        assert(syscall(SYS_sched_yield) == 0);
    }
    while (!atomic_load_explicit(&child_reparking, memory_order_acquire)) {
        assert(syscall(SYS_sched_yield) == 0);
    }
    for (unsigned int turn = 0; turn < 8; ++turn) {
        assert(syscall(SYS_sched_yield) == 0);
    }

    (void)syscall(SYS_exit_group, 0);
    return 92;
}
