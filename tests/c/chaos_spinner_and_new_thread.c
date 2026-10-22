/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A thread that never blocks, and a new thread it waits for
// (https://github.com/rrnewton/hermit/issues/4374).
//
// The main thread creates a worker that only sets a flag, then busy-waits on
// getpid until the flag is set. It prints how much CLOCK_MONOTONIC time passed
// from creating the worker to seeing the flag, in whole milliseconds:
// "worker ran after <N> ms". Under Hermit that is virtual time, so a given
// schedule always prints the same N; it measures how long the spinner kept
// the new thread waiting.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static atomic_int flag;

static void* worker(void* arg) {
  (void)arg;
  atomic_store(&flag, 1);
  return NULL;
}

static long long now_ns(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

int main(void) {
  long long start = now_ns();
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  while (!atomic_load(&flag)) {
    syscall(SYS_getpid);
  }
  long long waited = now_ns() - start;
  pthread_join(thread, NULL);
  printf("worker ran after %lld ms\n", waited / 1000000);
  return 0;
}
