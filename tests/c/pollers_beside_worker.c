/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A compute-bound worker beside four pollers that each loop on a 10 us
 * pthread_cond_timedwait nobody signals, so every wait times out.
 *
 * usage: pollers_beside_worker
 *
 * The worker runs 100 chunks of arithmetic and reads CLOCK_MONOTONIC after
 * each, then stops the pollers. Prints how long, in microseconds, the worker
 * took, the longest gap between two of its reads (how long it was held off
 * the CPU), and how many times the pollers' waits timed out.
 */

#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <time.h>

static atomic_int stop;
static atomic_long timeouts;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t never = PTHREAD_COND_INITIALIZER;
static volatile unsigned long sink;
static long long took_us;
static long long longest_gap_us;

static long long now_us(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (long long)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

static void* poller(void* arg) {
  (void)arg;
  while (!atomic_load(&stop)) {
    struct timespec until;
    clock_gettime(CLOCK_REALTIME, &until);
    until.tv_nsec += 10000;
    if (until.tv_nsec >= 1000000000) {
      until.tv_sec += 1;
      until.tv_nsec -= 1000000000;
    }
    pthread_mutex_lock(&lock);
    pthread_cond_timedwait(&never, &lock, &until);
    pthread_mutex_unlock(&lock);
    atomic_fetch_add(&timeouts, 1);
  }
  return NULL;
}

static void* worker(void* arg) {
  (void)arg;
  long long start = now_us();
  long long previous = start;
  for (int chunk = 0; chunk < 100; chunk++) {
    for (long i = 0; i < 1000000; i++) {
      sink += i;
    }
    long long t = now_us();
    if (t - previous > longest_gap_us) {
      longest_gap_us = t - previous;
    }
    previous = t;
  }
  took_us = previous - start;
  atomic_store(&stop, 1);
  return NULL;
}

int main(void) {
  pthread_t w, p[4];
  if (pthread_create(&w, NULL, worker, NULL) != 0) {
    return 2;
  }
  for (int i = 0; i < 4; i++) {
    if (pthread_create(&p[i], NULL, poller, NULL) != 0) {
      return 2;
    }
  }
  pthread_join(w, NULL);
  for (int i = 0; i < 4; i++) {
    pthread_join(p[i], NULL);
  }
  printf(
      "worker took %lld us, longest gap %lld us; %ld timeouts\n",
      took_us,
      longest_gap_us,
      atomic_load(&timeouts));
  return 0;
}
