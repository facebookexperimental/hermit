/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A sched_yield spinner that waits for a flag a sleeping thread sets.
//
// The main thread creates a worker that sleeps for 500 us and then sets a
// flag. With the argument "spin" the main thread calls sched_yield until the
// flag is set before it joins the worker; without it, it joins the worker at
// once. It prints "done". A schedule recorded without "spin" and replayed with
// it leaves its recording at the main thread's first sched_yield, where the
// record has the main thread wait in the join: from there the spinner can
// finish only once the worker has run.
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

static atomic_int flag;

static void* worker(void* arg) {
  (void)arg;
  struct timespec pause = {.tv_sec = 0, .tv_nsec = 500000};
  nanosleep(&pause, NULL);
  atomic_store(&flag, 1);
  return NULL;
}

int main(int argc, char** argv) {
  bool spin = argc > 1 && strcmp(argv[1], "spin") == 0;
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  while (spin && !atomic_load(&flag)) {
    sched_yield();
  }
  if (pthread_join(thread, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  printf("done\n");
  return 0;
}
