/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A guest that never ends, for `hermit run --virtual-timeout`. With no
// argument it prints a numbered tick and sleeps one second, forever; with a
// number N it does that N times and exits 0, a run a bound past its end never
// stops; with `term N` it starts a thread that waits in pause() forever, does
// the N ticks and then raises SIGTERM, so the process dies of a fatal signal
// while that thread is still parked; with `spin` it prints a marker and then
// spins in user code with no system call, forever. The spin tests a counter on
// every iteration so that it retires conditional branches, which is what
// Hermit's preemption timer counts; a loop with none would never be
// preempted, and a lone thread that makes no system call never returns to the
// scheduler that checks the bound. Natively the endless forms run until
// something kills them; under Hermit the virtual bound stops them at the same
// point in every run, and the ticks show the output written before the stop
// is kept.
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static void* parked(void* arg) {
  (void)arg;
  for (;;) {
    pause();
  }
  return NULL;
}

static void tick_for(unsigned long ticks, int forever) {
  for (unsigned long tick = 0; forever || tick < ticks; tick++) {
    printf("tick %lu\n", tick);
    fflush(stdout);
    struct timespec second = {1, 0};
    nanosleep(&second, NULL);
  }
}

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "spin") == 0) {
    printf("spinning\n");
    fflush(stdout);
    volatile unsigned long counter = 0;
    for (;;) {
      if (++counter == 0) {
        printf("the counter wrapped\n");
      }
    }
  }
  if (argc == 3 && strcmp(argv[1], "term") == 0) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, parked, NULL) != 0) {
      return 2;
    }
    tick_for(strtoul(argv[2], NULL, 10), 0);
    raise(SIGTERM);
    return 3;
  }
  tick_for(argc == 2 ? strtoul(argv[1], NULL, 10) : 0, argc < 2);
  return 0;
}
