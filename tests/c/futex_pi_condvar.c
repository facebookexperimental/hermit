/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A bounded queue guarded by a PTHREAD_PRIO_INHERIT mutex and two condition
 * variables, with two producers and two consumers. A condition variable waits
 * on its own futex words and then takes the mutex again, so a contended mutex
 * goes through FUTEX_LOCK_PI and FUTEX_UNLOCK_PI; every thread yields while it
 * holds the mutex so that the others find it taken. glibc's condition variables
 * have not used FUTEX_WAIT_REQUEUE_PI or FUTEX_CMP_REQUEUE_PI since glibc
 * 2.25. The totals are printed and must be the same natively and under Hermit.
 */

#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define PRODUCERS 2
#define CONSUMERS 2
#define ITEMS 150
#define CAPACITY 4

static pthread_mutex_t lock;
static pthread_cond_t not_full = PTHREAD_COND_INITIALIZER;
static pthread_cond_t not_empty = PTHREAD_COND_INITIALIZER;
static long queue[CAPACITY];
static int head;
static int length;
static int producers_left = PRODUCERS;
static long consumed[CONSUMERS];
static long sums[CONSUMERS];

static void check(int ret, const char* what) {
  if (ret != 0) {
    printf("%s: %s\n", what, strerrorname_np(ret));
    exit(1);
  }
}

static void* produce(void* arg) {
  long base = (long)arg * ITEMS;
  for (long i = 1; i <= ITEMS; i++) {
    check(pthread_mutex_lock(&lock), "producer lock");
    while (length == CAPACITY) {
      check(pthread_cond_wait(&not_full, &lock), "pthread_cond_wait");
    }
    queue[(head + length) % CAPACITY] = base + i;
    length++;
    sched_yield();
    check(pthread_cond_signal(&not_empty), "pthread_cond_signal");
    check(pthread_mutex_unlock(&lock), "producer unlock");
  }
  check(pthread_mutex_lock(&lock), "producer lock");
  producers_left--;
  check(pthread_cond_broadcast(&not_empty), "pthread_cond_broadcast");
  check(pthread_mutex_unlock(&lock), "producer unlock");
  return NULL;
}

static void* consume(void* arg) {
  long id = (long)arg;
  for (;;) {
    check(pthread_mutex_lock(&lock), "consumer lock");
    while (length == 0 && producers_left > 0) {
      check(pthread_cond_wait(&not_empty, &lock), "pthread_cond_wait");
    }
    if (length == 0) {
      check(pthread_mutex_unlock(&lock), "consumer unlock");
      return NULL;
    }
    long item = queue[head];
    head = (head + 1) % CAPACITY;
    length--;
    consumed[id]++;
    sums[id] += item;
    sched_yield();
    check(pthread_cond_signal(&not_full), "pthread_cond_signal");
    check(pthread_mutex_unlock(&lock), "consumer unlock");
  }
}

int main(void) {
  pthread_mutexattr_t attr;
  check(pthread_mutexattr_init(&attr), "pthread_mutexattr_init");
  check(
      pthread_mutexattr_setprotocol(&attr, PTHREAD_PRIO_INHERIT),
      "pthread_mutexattr_setprotocol");
  check(pthread_mutex_init(&lock, &attr), "pthread_mutex_init");

  pthread_t producers[PRODUCERS];
  pthread_t consumers[CONSUMERS];
  for (long i = 0; i < CONSUMERS; i++) {
    check(pthread_create(&consumers[i], NULL, consume, (void*)i), "pthread_create");
  }
  for (long i = 0; i < PRODUCERS; i++) {
    check(pthread_create(&producers[i], NULL, produce, (void*)i), "pthread_create");
  }
  for (int i = 0; i < PRODUCERS; i++) {
    check(pthread_join(producers[i], NULL), "pthread_join");
  }
  long items = 0;
  long sum = 0;
  for (int i = 0; i < CONSUMERS; i++) {
    check(pthread_join(consumers[i], NULL), "pthread_join");
    items += consumed[i];
    sum += sums[i];
  }
  long expected = 0;
  for (long i = 1; i <= (long)PRODUCERS * ITEMS; i++) {
    expected += i;
  }
  printf(
      "producers %d, consumers %d: items %ld, sum %s, queue left %d\n",
      PRODUCERS,
      CONSUMERS,
      items,
      sum == expected ? "as produced" : "wrong",
      length);
  return items == (long)PRODUCERS * ITEMS && sum == expected ? 0 : 1;
}
