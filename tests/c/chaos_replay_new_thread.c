/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A thread created mid-run that races the thread that created it.
//
// The main thread computes, creates a worker, and both compute the same
// amount. The first to finish claims a shared slot, and each notes the
// CLOCK_MONOTONIC time it finished at. The main thread prints which thread
// finished first and both finishing times, in microseconds since it started,
// and exits 1 when the worker finished first. Under Hermit the times are
// virtual and the winner depends on the schedule: a replay that follows its
// recorded schedule prints the same line and exits with the same status, and a
// replay that adds a scheduler turn shifts a time, because every committed
// turn advances the virtual clock.
//
// With the argument "syscalls", both threads also make a getppid call every
// 256 rounds, so slices also end at syscalls, where the virtual time a
// syscall adds crosses a slice end, and not only at the preemption timer.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define ROUNDS 200000UL

enum { NOBODY, MAIN, WORKER };

static atomic_int first = NOBODY;
static bool syscalls;
static long long start_us;
static long long worker_done_us;

static long long now_us(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (long long)ts.tv_sec * 1000000LL + ts.tv_nsec / 1000;
}

// Branch-heavy work, so the preemption timer ends many slices in it.
static unsigned long compute(void) {
  unsigned long x = 1;
  unsigned long high = 0;
  for (unsigned long i = 0; i < ROUNDS; i++) {
    x = x * 6364136223846793005UL + 1442695040888963407UL;
    if (x >> 63) {
      high++;
    }
    if (syscalls && (i & 255) == 0) {
      syscall(SYS_getppid);
    }
  }
  return high;
}

static void claim(int me) {
  int nobody = NOBODY;
  atomic_compare_exchange_strong(&first, &nobody, me);
}

static void* worker(void* arg) {
  (void)arg;
  volatile unsigned long sink = compute();
  (void)sink;
  claim(WORKER);
  worker_done_us = now_us() - start_us;
  return NULL;
}

int main(int argc, char** argv) {
  syscalls = argc > 1 && strcmp(argv[1], "syscalls") == 0;
  start_us = now_us();
  volatile unsigned long sink = compute();
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL) != 0) {
    perror("pthread_create");
    return 2;
  }
  sink = compute();
  (void)sink;
  claim(MAIN);
  long long main_done_us = now_us() - start_us;
  if (pthread_join(thread, NULL) != 0) {
    perror("pthread_join");
    return 2;
  }
  int winner = atomic_load(&first);
  printf(
      "%s finished first; main at %lld us, worker at %lld us\n",
      winner == MAIN ? "main" : "worker",
      main_done_us,
      worker_done_us);
  return winner == WORKER;
}
