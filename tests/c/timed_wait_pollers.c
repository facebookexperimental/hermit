/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * CPU-bound work beside threads that poll with short timed waits, the idle
 * loop of many thread pools.
 *
 * usage: timed_wait_pollers MODE POLLERS WAIT_US ITERATIONS [SYSCALL_EVERY]
 *   MODE      condwait: each poller loops on pthread_cond_timedwait with a
 *             CLOCK_MONOTONIC deadline WAIT_US ahead, on one mutex and
 *             condition variable shared by all pollers (what C++
 *             condition_variable::wait_for does); sleep: each poller loops on
 *             clock_nanosleep(WAIT_US).
 *   POLLERS   number of polling threads (0 for the baseline).
 *   ITERATIONS
 *             length of main's CPU loop.
 *   SYSCALL_EVERY
 *             if given and positive, main's loop calls getppid every
 *             SYSCALL_EVERY iterations; otherwise it makes no system call.
 *
 * Prints work_us (main's loop), total_us (from before the first poller starts
 * to after the last one is joined) and polls (timed waits that returned),
 * all from CLOCK_MONOTONIC.
 */

#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond;
static atomic_bool stop;
static atomic_long polls;
static long wait_us;

static long long now_us(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (long long)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

static void* condwait_poller(void* arg) {
  (void)arg;
  pthread_mutex_lock(&lock);
  while (!atomic_load(&stop)) {
    struct timespec deadline;
    clock_gettime(CLOCK_MONOTONIC, &deadline);
    deadline.tv_nsec += wait_us * 1000;
    deadline.tv_sec += deadline.tv_nsec / 1000000000;
    deadline.tv_nsec %= 1000000000;
    int rc = pthread_cond_timedwait(&cond, &lock, &deadline);
    if (rc != 0 && rc != ETIMEDOUT) {
      fprintf(stderr, "pthread_cond_timedwait: %s\n", strerror(rc));
      exit(2);
    }
    atomic_fetch_add(&polls, 1);
  }
  pthread_mutex_unlock(&lock);
  return NULL;
}

static void* sleep_poller(void* arg) {
  (void)arg;
  while (!atomic_load(&stop)) {
    struct timespec ts = {.tv_sec = 0, .tv_nsec = wait_us * 1000};
    while (clock_nanosleep(CLOCK_MONOTONIC, 0, &ts, &ts) == EINTR) {
    }
    atomic_fetch_add(&polls, 1);
  }
  return NULL;
}

int main(int argc, char** argv) {
  if (argc != 5 && argc != 6) {
    fprintf(stderr, "usage: %s condwait|sleep POLLERS WAIT_US ITERATIONS [SYSCALL_EVERY]\n", argv[0]);
    return 2;
  }
  void* (*poller)(void*);
  if (strcmp(argv[1], "condwait") == 0) {
    poller = condwait_poller;
  } else if (strcmp(argv[1], "sleep") == 0) {
    poller = sleep_poller;
  } else {
    fprintf(stderr, "unknown mode %s\n", argv[1]);
    return 2;
  }
  int count = atoi(argv[2]);
  wait_us = atol(argv[3]);
  long iterations = atol(argv[4]);
  long syscall_every = argc == 6 ? atol(argv[5]) : 0;
  if (count < 0 || count > 64 || wait_us <= 0 || wait_us >= 1000000 || iterations < 0) {
    fprintf(stderr, "bad arguments\n");
    return 2;
  }
  pthread_condattr_t attr;
  pthread_condattr_init(&attr);
  pthread_condattr_setclock(&attr, CLOCK_MONOTONIC);
  pthread_cond_init(&cond, &attr);

  pthread_t threads[64];
  long long start = now_us();
  for (int i = 0; i < count; i++) {
    if (pthread_create(&threads[i], NULL, poller, NULL) != 0) {
      fprintf(stderr, "pthread_create failed\n");
      return 2;
    }
  }
  volatile double x = 0;
  long long work_start = now_us();
  for (long i = 0; i < iterations; i++) {
    x += i * 1e-9;
    if (syscall_every > 0 && i % syscall_every == 0) {
      getppid();
    }
  }
  long long work_end = now_us();
  atomic_store(&stop, 1);
  pthread_mutex_lock(&lock);
  pthread_cond_broadcast(&cond);
  pthread_mutex_unlock(&lock);
  for (int i = 0; i < count; i++) {
    pthread_join(threads[i], NULL);
  }
  long long end = now_us();
  printf(
      "work_us=%lld total_us=%lld polls=%ld\n",
      work_end - work_start,
      end - start,
      atomic_load(&polls));
  return 0;
}
