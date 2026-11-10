/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A robust PTHREAD_PRIO_INHERIT mutex whose owner exits while holding it.
 * The kernel's robust-list walk marks the word FUTEX_OWNER_DIED, and the next
 * thread to lock it gets EOWNERDEAD, makes it consistent and goes on:
 *
 * 1. no waiter: the next pthread_mutex_lock finds the word marked and enters
 *    the kernel with FUTEX_LOCK_PI, which takes it;
 * 2. no waiter, through pthread_mutex_trylock, which enters the kernel with
 *    FUTEX_TRYLOCK_PI for a word marked FUTEX_OWNER_DIED;
 * 3. a waiter blocked in FUTEX_LOCK_PI when the owner exits: the kernel hands
 *    it the lock, and it writes its own TID, keeping FUTEX_OWNER_DIED.
 *
 * Every result is printed and must be the same natively and under Hermit.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static pthread_mutex_t lock;
static int owner_holds;

static const char* name(int ret) {
  return ret == 0 ? "0" : strerrorname_np(ret);
}

static void check(int ret, const char* what) {
  if (ret != 0) {
    printf("%s: %s\n", what, name(ret));
    exit(1);
  }
}

/* glibc keeps the futex word of a mutex in `__data.__lock`. */
static unsigned word(void) {
  return __atomic_load_n((unsigned*)&lock.__data.__lock, __ATOMIC_SEQ_CST);
}

static void* lock_and_exit(void* arg) {
  int wait_for_a_waiter = *(int*)arg;
  check(pthread_mutex_lock(&lock), "owner lock");
  __atomic_store_n(&owner_holds, 1, __ATOMIC_SEQ_CST);
  while (wait_for_a_waiter && !(word() & FUTEX_WAITERS)) {
    sched_yield();
  }
  return NULL;
}

static void abandon(int wait_for_a_waiter, pthread_t* owner) {
  static int arg[2] = {0, 1};
  __atomic_store_n(&owner_holds, 0, __ATOMIC_SEQ_CST);
  check(
      pthread_create(owner, NULL, lock_and_exit, &arg[wait_for_a_waiter]),
      "pthread_create");
  while (!__atomic_load_n(&owner_holds, __ATOMIC_SEQ_CST)) {
    sched_yield();
  }
}

static void recover(const char* how, int ret) {
  printf("%s: %s", how, name(ret));
  if (ret == EOWNERDEAD) {
    printf(", consistent %s", name(pthread_mutex_consistent(&lock)));
    printf(", unlock %s", name(pthread_mutex_unlock(&lock)));
  }
  int again = pthread_mutex_lock(&lock);
  printf(", lock again %s", name(again));
  printf(", unlock %s", name(pthread_mutex_unlock(&lock)));
  printf(", word %#x\n", word());
}

int main(void) {
  pthread_mutexattr_t attr;
  check(pthread_mutexattr_init(&attr), "pthread_mutexattr_init");
  check(
      pthread_mutexattr_setprotocol(&attr, PTHREAD_PRIO_INHERIT),
      "pthread_mutexattr_setprotocol");
  check(
      pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST),
      "pthread_mutexattr_setrobust");
  check(pthread_mutex_init(&lock, &attr), "pthread_mutex_init");

  pthread_t owner;
  abandon(0, &owner);
  check(pthread_join(owner, NULL), "pthread_join");
  printf(
      "owner exited holding it, no waiter: word %s\n",
      word() == FUTEX_OWNER_DIED ? "OWNER_DIED" : "unexpected");
  recover("pthread_mutex_lock", pthread_mutex_lock(&lock));

  abandon(0, &owner);
  check(pthread_join(owner, NULL), "pthread_join");
  recover("pthread_mutex_trylock", pthread_mutex_trylock(&lock));

  abandon(1, &owner);
  int ret = pthread_mutex_lock(&lock);
  check(pthread_join(owner, NULL), "pthread_join");
  recover("pthread_mutex_lock blocked when the owner exited", ret);

  check(pthread_mutex_destroy(&lock), "pthread_mutex_destroy");
  return 0;
}
