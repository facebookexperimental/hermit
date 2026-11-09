/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Guest for replays of slices that ended at a syscall's prehook (the PMU
// cases that compile it in hermit-cli/tests/cli.rs).
//
// A sleeper keeps a timed wait pending, so a chaos run ends the worker's
// slices at that deadline. The worker computes between syscalls and only
// checks its slice at a syscall, so such a slice ends at the prehook of the
// syscall after the deadline. That prehook shares its retired-branch count and
// its virtual time with the instructions after the worker's last branch
// before the syscall, which is where a PMU stop on that count lands. The
// guest prints the virtual clock last, which a replay reproduces only if it
// takes the scheduler turns the recording took.

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>

enum { SLEEPS = 40, ROUNDS = 400, WORK = 3000 };

static volatile uint64_t sink;

static void* sleeper(void* arg) {
  (void)arg;
  const struct timespec nap = {.tv_sec = 0, .tv_nsec = 2000000};
  for (int i = 0; i < SLEEPS; i++) {
    nanosleep(&nap, NULL);
  }
  return NULL;
}

int main(void) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, sleeper, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  uint64_t written = 0;
  for (int round = 0; round < ROUNDS; round++) {
    for (int i = 0; i < WORK; i++) {
      sink += (uint64_t)i * UINT64_C(2654435761);
    }
    written += (uint64_t)write(STDOUT_FILENO, ".", 1);
  }
  if (pthread_join(thread, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  // The virtual clock a replay reproduces only if it reproduces every
  // scheduler turn the recording took.
  struct timespec now;
  if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
    perror("clock_gettime");
    return 1;
  }
  printf("\nsyscall-slice-end-ok %llu at %lld.%09ld\n", (unsigned long long)written,
         (long long)now.tv_sec, now.tv_nsec);
  return 0;
}
