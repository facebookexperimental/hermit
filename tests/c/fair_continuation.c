/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * fair_continuation: one thread writes ROUNDS lines to stdout (argument 1,
 * default 10), one write(2) each, beside a thread that only reads the clock
 * until the writer is done. Then the main thread prints how many
 * milliseconds of CLOCK_MONOTONIC the rounds took.
 *
 * By default the main thread writes and a second thread spins. With a second
 * argument `spinner-main`, the main thread spins and a second thread writes,
 * sleeping 2 milliseconds after each line, longer than a scheduler turn
 * charges, so that each sleep blocks and the writer is woken.
 *
 * Without --chaos every thread has the same priority, and each write is a
 * scheduler turn that sends the writer to the back of the queue: the spinner
 * then keeps the CPU for a whole timeslice, so the rounds take about ROUNDS
 * timeslices. A scheduler that runs the thread that has run the least virtual
 * time first (as --replay-continue-fairly does past the end of a replayed
 * schedule) runs the writer's lines back to back once the spinner has had one
 * timeslice, so the rounds take about one.
 */

#include <limits.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static atomic_int stop;
static int rounds = 10;
static int writer_sleeps;
static int completed_sleeps;
static long long shortest_sleep_us = LLONG_MAX;
static long long longest_sleep_us;

static long long monotonic_us(void) {
  struct timespec now;
  clock_gettime(CLOCK_MONOTONIC, &now);
  return (long long)now.tv_sec * 1000000 + now.tv_nsec / 1000;
}

static void write_rounds(void) {
  for (int round = 0; round < rounds; round++) {
    char line[32];
    int length = snprintf(line, sizeof line, "round %d\n", round);
    if (write(STDOUT_FILENO, line, (size_t)length) != length) {
      perror("write");
      exit(1);
    }
    if (writer_sleeps) {
      struct timespec pause = {0, 2000000};
      long long start = monotonic_us();
      if (nanosleep(&pause, NULL) != 0) {
        perror("nanosleep");
        exit(1);
      }
      long long elapsed = monotonic_us() - start;
      completed_sleeps++;
      if (elapsed < shortest_sleep_us) shortest_sleep_us = elapsed;
      if (elapsed > longest_sleep_us) longest_sleep_us = elapsed;
    }
  }
}

static void spin_until_stopped(void) {
  struct timespec now;
  while (!atomic_load(&stop)) {
    clock_gettime(CLOCK_MONOTONIC, &now);
  }
}

static void* spin(void* arg) {
  (void)arg;
  spin_until_stopped();
  return NULL;
}

static void* write_then_stop(void* arg) {
  (void)arg;
  write_rounds();
  atomic_store(&stop, 1);
  return NULL;
}

static long long monotonic_ms(void) {
  struct timespec now;
  clock_gettime(CLOCK_MONOTONIC, &now);
  return (long long)now.tv_sec * 1000 + now.tv_nsec / 1000000;
}

int main(int argc, char** argv) {
  rounds = argc > 1 ? atoi(argv[1]) : 10;
  int spinner_main = argc > 2 && strcmp(argv[2], "spinner-main") == 0;
  writer_sleeps = spinner_main;
  long long start = monotonic_ms();
  pthread_t other;
  if (pthread_create(&other, NULL, spinner_main ? write_then_stop : spin, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  if (spinner_main) {
    spin_until_stopped();
  } else {
    write_rounds();
    atomic_store(&stop, 1);
  }
  pthread_join(other, NULL);
  long long elapsed = monotonic_ms() - start;
  printf("rounds took %lld ms\n", elapsed);
  // Report only after the original measurement: an extra write while the
  // spinner runs would itself give up a scheduler turn.
  if (writer_sleeps) {
    printf("sleeps %d min_us %lld max_us %lld\n", completed_sleeps,
           shortest_sleep_us, longest_sleep_us);
  }
  return 0;
}
