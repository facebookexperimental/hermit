/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A thread that sleeps for less than a scheduler turn charge in a loop, and
 * creates the thread that waits behind it.
 *
 * Thread S first spins on the CPU for about a second of virtual time, long
 * enough to cross several timeslice ends: under --chaos each is a change
 * point, where S draws a new priority. S then creates thread C and loops on
 * nanosleep(100 us) until C stops it, or until it has slept 100000 times.
 * C reads CLOCK_MONOTONIC as soon as it runs and stops S. Prints how long,
 * in microseconds, C waited for its first instruction, and how many times
 * S slept.
 */

#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#define SPIN 50000000L
#define SLEEP_LIMIT 100000L

static atomic_int stop;
static atomic_long sleeps;
static long long created_us;
static long long first_us;

static long long now_us(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (long long)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

static void* newcomer(void* arg) {
  (void)arg;
  first_us = now_us();
  atomic_store(&stop, 1);
  return NULL;
}

static void* sleeper(void* arg) {
  pthread_t* c = arg;
  volatile long sink = 0;
  for (long i = 0; i < SPIN; i++) {
    if (i % 7 == 3) {
      sink += i;
    }
  }
  created_us = now_us();
  if (pthread_create(c, NULL, newcomer, NULL) != 0) {
    perror("pthread_create");
    exit(2);
  }
  struct timespec pause = {.tv_sec = 0, .tv_nsec = 100000};
  while (!atomic_load(&stop) && atomic_load(&sleeps) < SLEEP_LIMIT) {
    nanosleep(&pause, NULL);
    atomic_fetch_add(&sleeps, 1);
  }
  return NULL;
}

int main(void) {
  pthread_t s, c;
  if (pthread_create(&s, NULL, sleeper, &c) != 0) {
    perror("pthread_create");
    return 2;
  }
  pthread_join(s, NULL);
  pthread_join(c, NULL);
  printf(
      "new thread waited %lld us; %ld sleeps\n",
      first_us - created_us,
      atomic_load(&sleeps));
  return 0;
}
