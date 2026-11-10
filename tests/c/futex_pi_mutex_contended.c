/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Four threads take a PTHREAD_PRIO_INHERIT mutex many times each and yield
 * while holding it, so the others find it taken. glibc takes and releases an
 * uncontended priority-inheritance mutex with its own compare-and-exchange and
 * calls FUTEX_LOCK_PI and FUTEX_UNLOCK_PI only under contention, so this guest
 * exercises those commands. Every thread checks that it is alone inside the
 * critical section; the final count and the number of overlaps are printed and
 * must be the same natively and under Hermit. Main holds the mutex while it
 * starts the threads and until one has queued in FUTEX_LOCK_PI, which sets
 * FUTEX_WAITERS in the mutex's word, so at least one lock is contended in
 * every run.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define THREADS 4
#define ITERATIONS 100

static pthread_mutex_t lock;
static int inside;
static long count;
static long overlaps;

static void check(int ret, const char* what) {
  if (ret != 0) {
    printf("%s: %s\n", what, strerrorname_np(ret));
    exit(1);
  }
}

static void* worker(void* arg) {
  (void)arg;
  for (int i = 0; i < ITERATIONS; i++) {
    check(pthread_mutex_lock(&lock), "pthread_mutex_lock");
    if (inside++ != 0) {
      overlaps++;
    }
    long seen = count;
    sched_yield();
    count = seen + 1;
    inside--;
    check(pthread_mutex_unlock(&lock), "pthread_mutex_unlock");
    if (i % 7 == 0) {
      sched_yield();
    }
  }
  return NULL;
}

int main(void) {
  pthread_mutexattr_t attr;
  check(pthread_mutexattr_init(&attr), "pthread_mutexattr_init");
  check(
      pthread_mutexattr_setprotocol(&attr, PTHREAD_PRIO_INHERIT),
      "pthread_mutexattr_setprotocol");
  check(pthread_mutex_init(&lock, &attr), "pthread_mutex_init");

  pthread_t threads[THREADS];
  check(pthread_mutex_lock(&lock), "pthread_mutex_lock");
  for (int i = 0; i < THREADS; i++) {
    check(pthread_create(&threads[i], NULL, worker, NULL), "pthread_create");
  }
  while (!(__atomic_load_n(&lock.__data.__lock, __ATOMIC_SEQ_CST) & FUTEX_WAITERS)) {
    sched_yield();
  }
  check(pthread_mutex_unlock(&lock), "pthread_mutex_unlock");
  for (int i = 0; i < THREADS; i++) {
    check(pthread_join(threads[i], NULL), "pthread_join");
  }
  check(pthread_mutex_destroy(&lock), "pthread_mutex_destroy");
  printf(
      "threads %d, iterations %d: count %ld, overlaps %ld\n",
      THREADS,
      ITERATIONS,
      count,
      overlaps);
  return count == (long)THREADS * ITERATIONS && overlaps == 0 ? 0 : 1;
}
