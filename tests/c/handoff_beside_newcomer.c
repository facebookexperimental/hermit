/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A newly created thread beside two threads that pass the CPU back and forth
 * through a condition variable.
 *
 * usage: handoff_beside_newcomer [HANDOFFS_BEFORE]
 *
 * Threads A and B hand a turn variable back and forth under one mutex,
 * waiting on one condition variable without a timeout, until told to stop.
 * Main waits until they have made HANDOFFS_BEFORE (default 1000) hand-offs,
 * creates thread C, and joins it. C reads CLOCK_MONOTONIC once it runs and
 * stops A and B. Prints how long, in microseconds, C waited for its first
 * instruction after main created it, and how many hand-offs happened in all.
 */

#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int turn;
static int stop;
static atomic_long handoffs;
static long long created_us;
static long long first_run_us;

static long long now_us(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (long long)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

static void* player(void* arg) {
  int me = (int)(long)arg;
  pthread_mutex_lock(&lock);
  while (!stop) {
    if (turn == me) {
      turn = 1 - me;
      atomic_fetch_add(&handoffs, 1);
      pthread_cond_broadcast(&cond);
    } else {
      pthread_cond_wait(&cond, &lock);
    }
  }
  pthread_mutex_unlock(&lock);
  return NULL;
}

static void* newcomer(void* arg) {
  (void)arg;
  first_run_us = now_us();
  pthread_mutex_lock(&lock);
  stop = 1;
  pthread_cond_broadcast(&cond);
  pthread_mutex_unlock(&lock);
  return NULL;
}

int main(int argc, char** argv) {
  long before = argc > 1 ? atol(argv[1]) : 1000;
  pthread_t a, b, c;
  if (pthread_create(&a, NULL, player, (void*)0L) != 0 ||
      pthread_create(&b, NULL, player, (void*)1L) != 0) {
    return 2;
  }
  while (atomic_load(&handoffs) < before) {
    struct timespec ms = {.tv_sec = 0, .tv_nsec = 1000000};
    nanosleep(&ms, NULL);
  }
  created_us = now_us();
  if (pthread_create(&c, NULL, newcomer, NULL) != 0) {
    return 2;
  }
  pthread_join(c, NULL);
  pthread_join(a, NULL);
  pthread_join(b, NULL);
  printf(
      "newcomer waited %lld us; %ld handoffs\n",
      first_run_us - created_us,
      atomic_load(&handoffs));
  return 0;
}
