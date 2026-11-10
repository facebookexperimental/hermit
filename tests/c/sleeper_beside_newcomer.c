/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A thread that sleeps for less than a scheduler turn charge in a loop, beside
 * a thread that main creates later, or beside one that is already running.
 *
 * usage: sleeper_beside_newcomer [existing|zero]
 *
 * Thread S loops on nanosleep(100 us) until told to stop.
 *   (no argument): main waits until S has slept 200 times, creates thread C
 *     and joins it. C reads CLOCK_MONOTONIC as soon as it runs and stops S.
 *     Prints how long, in microseconds, C waited for its first instruction.
 *   existing: main creates thread C, which sleeps 50 ms, and then S, and
 *     joins C. C stops S when it wakes. Prints how late, in microseconds, C
 *     woke after its 50 ms.
 *   zero: as existing, but S loops on nanosleep of zero, a yield idiom.
 * Each also prints how many times S slept.
 */

#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

static atomic_int stop;
static atomic_long sleeps;
static struct timespec interval = {.tv_sec = 0, .tv_nsec = 100000};
static long long created_us;
static long long first_us;

static long long now_us(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (long long)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

static void* sleeper(void* arg) {
  (void)arg;
  while (!atomic_load(&stop)) {
    nanosleep(&interval, NULL);
    atomic_fetch_add(&sleeps, 1);
  }
  return NULL;
}

static void* napper(void* arg) {
  (void)arg;
  struct timespec nap = {.tv_sec = 0, .tv_nsec = 50000000};
  created_us = now_us();
  nanosleep(&nap, NULL);
  first_us = now_us();
  atomic_store(&stop, 1);
  return NULL;
}

static void* newcomer(void* arg) {
  (void)arg;
  first_us = now_us();
  atomic_store(&stop, 1);
  return NULL;
}

int main(int argc, char** argv) {
  pthread_t s, c;
  if (argc > 1) {
    int zero = strcmp(argv[1], "existing") != 0;
    if (zero && strcmp(argv[1], "zero") != 0) {
      return 2;
    }
    if (zero) {
      interval.tv_nsec = 0;
    }
    if (pthread_create(&c, NULL, napper, NULL) != 0 ||
        pthread_create(&s, NULL, sleeper, NULL) != 0) {
      return 2;
    }
    pthread_join(c, NULL);
    pthread_join(s, NULL);
    printf(
        "existing thread woke %lld us late; %ld sleeps\n",
        first_us - created_us - 50000,
        atomic_load(&sleeps));
    return 0;
  }
  if (pthread_create(&s, NULL, sleeper, NULL) != 0) {
    return 2;
  }
  while (atomic_load(&sleeps) < 200) {
    struct timespec ms = {.tv_sec = 0, .tv_nsec = 1000000};
    nanosleep(&ms, NULL);
  }
  created_us = now_us();
  if (pthread_create(&c, NULL, newcomer, NULL) != 0) {
    return 2;
  }
  pthread_join(c, NULL);
  pthread_join(s, NULL);
  printf(
      "new thread waited %lld us; %ld sleeps\n",
      first_us - created_us,
      atomic_load(&sleeps));
  return 0;
}
