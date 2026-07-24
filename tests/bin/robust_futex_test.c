/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Reproducer for robust-futex owner-death wakeups.
 *
 * Build:
 *   cc -O2 -Wall -Wextra -Werror -pthread \
 *     tests/bin/robust_futex_test.c -o robust_futex_test
 *
 * Native Linux must print PASS and exit 0. Under Hermit's precise futex model,
 * the waiter is held in Detcore's queue rather than the kernel futex queue.
 * Kernel robust-list cleanup marks the mutex FUTEX_OWNER_DIED, but its internal
 * wake cannot reach Detcore's waiter. The strict run therefore exposes the
 * missing owner-death bridge instead of printing PASS.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

#if !defined(__GLIBC__)
#error "This reproducer relies on glibc's pthread_mutex_t futex word layout"
#endif

enum {
  OWNER_GET_ROBUST_LIST_FAILED = 10,
  OWNER_INVALID_ROBUST_LIST = 11,
  OWNER_SET_ROBUST_LIST_FAILED = 12,
  OWNER_LOCK_FAILED = 13,
  OWNER_WAITER_NOT_BLOCKED = 14,
  WAITER_LOCK_RESULT_WRONG = 20,
  WAITER_CONSISTENT_FAILED = 21,
  WAITER_UNLOCK_FAILED = 22,
};

static pthread_mutex_t mutex;
static atomic_bool owner_locked = false;
static atomic_bool waiter_started = false;

static void *thread_result(int code) {
  return (void *)(uintptr_t)code;
}

static void *owner_thread(void *unused) {
  (void)unused;

  struct robust_list_head *head = NULL;
  size_t len = 0;
  if (syscall(SYS_get_robust_list, 0, &head, &len) != 0) {
    perror("get_robust_list");
    return thread_result(OWNER_GET_ROBUST_LIST_FAILED);
  }
  if (head == NULL || len != sizeof(*head)) {
    fprintf(stderr, "unexpected robust-list registration\n");
    return thread_result(OWNER_INVALID_ROBUST_LIST);
  }

  /* Explicitly exercise set_robust_list using glibc's registered list head. */
  if (syscall(SYS_set_robust_list, head, len) != 0) {
    perror("set_robust_list");
    return thread_result(OWNER_SET_ROBUST_LIST_FAILED);
  }

  int ret = pthread_mutex_lock(&mutex);
  if (ret != 0) {
    fprintf(stderr, "owner pthread_mutex_lock: %d\n", ret);
    return thread_result(OWNER_LOCK_FAILED);
  }
  atomic_store_explicit(&owner_locked, true, memory_order_release);

  while (!atomic_load_explicit(&waiter_started, memory_order_acquire)) {
    sched_yield();
  }

  /*
   * A waiter that starts after this thread exits can observe EOWNERDEAD without
   * requiring a wake. Wait for glibc to set FUTEX_WAITERS in the mutex word so
   * this test specifically requires an owner-death wakeup.
   */
  for (int attempts = 0; attempts < 1000000; ++attempts) {
    int lock_word = __atomic_load_n(&mutex.__data.__lock, __ATOMIC_ACQUIRE);
    if (((unsigned int)lock_word & FUTEX_WAITERS) != 0) {
      return NULL; /* Exit while still owning mutex. */
    }
    sched_yield();
  }

  fprintf(stderr, "waiter never set FUTEX_WAITERS\n");
  return thread_result(OWNER_WAITER_NOT_BLOCKED);
}

static void *waiter_thread(void *unused) {
  (void)unused;

  while (!atomic_load_explicit(&owner_locked, memory_order_acquire)) {
    sched_yield();
  }
  atomic_store_explicit(&waiter_started, true, memory_order_release);

  int ret = pthread_mutex_lock(&mutex);
  if (ret != EOWNERDEAD) {
    fprintf(stderr,
            "waiter pthread_mutex_lock: expected EOWNERDEAD (%d), got %d\n",
            EOWNERDEAD, ret);
    if (ret == 0) {
      pthread_mutex_unlock(&mutex);
    }
    return thread_result(WAITER_LOCK_RESULT_WRONG);
  }

  ret = pthread_mutex_consistent(&mutex);
  if (ret != 0) {
    fprintf(stderr, "pthread_mutex_consistent: %d\n", ret);
    return thread_result(WAITER_CONSISTENT_FAILED);
  }
  ret = pthread_mutex_unlock(&mutex);
  if (ret != 0) {
    fprintf(stderr, "waiter pthread_mutex_unlock: %d\n", ret);
    return thread_result(WAITER_UNLOCK_FAILED);
  }
  return NULL;
}

static void check_pthread(int ret, const char *operation) {
  if (ret != 0) {
    fprintf(stderr, "%s: %d\n", operation, ret);
    exit(EXIT_FAILURE);
  }
}

int main(void) {
  pthread_mutexattr_t attr;
  check_pthread(pthread_mutexattr_init(&attr), "pthread_mutexattr_init");
  check_pthread(pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST),
                "pthread_mutexattr_setrobust");
  check_pthread(pthread_mutex_init(&mutex, &attr), "pthread_mutex_init");
  check_pthread(pthread_mutexattr_destroy(&attr), "pthread_mutexattr_destroy");

  pthread_t owner;
  pthread_t waiter;
  check_pthread(pthread_create(&owner, NULL, owner_thread, NULL),
                "pthread_create(owner)");
  check_pthread(pthread_create(&waiter, NULL, waiter_thread, NULL),
                "pthread_create(waiter)");

  void *owner_result = NULL;
  void *waiter_result = NULL;
  check_pthread(pthread_join(owner, &owner_result), "pthread_join(owner)");
  check_pthread(pthread_join(waiter, &waiter_result), "pthread_join(waiter)");

  if (owner_result != NULL || waiter_result != NULL) {
    fprintf(stderr, "owner result=%lu, waiter result=%lu\n",
            (unsigned long)(uintptr_t)owner_result,
            (unsigned long)(uintptr_t)waiter_result);
    return EXIT_FAILURE;
  }

  check_pthread(pthread_mutex_destroy(&mutex), "pthread_mutex_destroy");
  puts("PASS: robust mutex waiter received EOWNERDEAD");
  return EXIT_SUCCESS;
}
