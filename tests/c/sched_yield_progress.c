/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Regression guest for GH #81: chaos must not starve sched_yield loops when
 * timer preemption is disabled.
 *
 * The main thread spins on an atomic flag, calling sched_yield() while it
 * waits. A worker thread does a small amount of work and then publishes a
 * value and sets the flag. Under `--chaos --preemption-timeout=disabled`,
 * priorities are fixed at thread creation and only re-randomized at timer
 * preemptions, which are off. Before the fix, a spinning sched_yield loop that
 * happened to hold the highest priority would monopolize the single logical CPU
 * and the worker would never run, hanging until an external timeout. With the
 * fix, sched_yield is treated as a chaos reprioritization point, so the worker
 * eventually runs and the program makes progress.
 *
 * Success is printing "sched-yield-progress-ok <value>" and exiting 0. Failure
 * is a hang (caught by the harness timeout).
 */

#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>

static atomic_int g_ready = 0;
static atomic_ullong g_value = 0;

static void* worker(void* arg) {
  (void)arg;
  /* A bit of real work so the worker cannot complete in the same turn it is
   * created; this widens the window in which the main thread would starve it. */
  unsigned long long acc = 0;
  for (unsigned long long i = 0; i < 2000000ULL; i++) {
    acc += i;
  }
  atomic_store_explicit(&g_value, acc | 1ULL, memory_order_seq_cst);
  atomic_store_explicit(&g_ready, 1, memory_order_release);
  return NULL;
}

int main(void) {
  pthread_t t;
  if (pthread_create(&t, NULL, worker, NULL) != 0) {
    fprintf(stderr, "pthread_create failed\n");
    return 2;
  }

  /* Spin waiting for the worker, yielding the CPU on every iteration. */
  while (atomic_load_explicit(&g_ready, memory_order_acquire) == 0) {
    sched_yield();
  }

  if (pthread_join(t, NULL) != 0) {
    fprintf(stderr, "pthread_join failed\n");
    return 3;
  }

  unsigned long long value = atomic_load_explicit(&g_value, memory_order_seq_cst);
  printf("sched-yield-progress-ok %llu\n", value);
  return 0;
}
