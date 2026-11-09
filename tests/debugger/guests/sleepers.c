/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A guest that never stops on its own, for gdb's interrupt: two threads sleep
 * in a loop, one keeps yielding, and main waits for the first sleeper, which
 * never ends. Under Hermit no thread is on the CPU most of the time: each
 * waits in Detcore for its turn or for its sleep to end in virtual time.
 */

#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <unistd.h>

#define SLEEPERS 2

static volatile unsigned long yields;

static void* sleeper(void* arg) {
  for (;;) {
    sleep(1);
  }
  return arg;
}

static void* yielder(void* arg) {
  for (;;) {
    yields++;
    sched_yield();
  }
  return arg;
}

int main(void) {
  pthread_t threads[SLEEPERS + 1];
  for (int i = 0; i < SLEEPERS; i++) {
    if (pthread_create(&threads[i], NULL, sleeper, NULL) != 0) {
      return 1;
    }
  }
  if (pthread_create(&threads[SLEEPERS], NULL, yielder, NULL) != 0) {
    return 1;
  }
  printf("all threads started\n");
  fflush(stdout);
  pthread_join(threads[0], NULL);
  return 0;
}
