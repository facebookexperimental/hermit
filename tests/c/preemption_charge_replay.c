/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Threads that each make many short syscalls, for comparing a recording's
// clock with its schedule replay's.
//
// Usage: preemption_charge_replay CALLS THREADS (THREADS at most 4). Each
// thread makes CALLS getppid calls; the main thread joins them and prints the
// CLOCK_MONOTONIC time each thread took and the time from the first create to
// the last join, in nanoseconds, on one line:
// "total_ns=<N> t0_ns=<N> ...". Under Hermit these are virtual times, so a
// replay that charges any turn differently from its recording prints another
// line. A logical --target-timeslice shorter than a syscall's virtual cost
// ends a slice at nearly every call, so most turns end an expired slice.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

enum { MAX_THREADS = 4 };

static long calls;
static long long took_ns[MAX_THREADS];

static long long now_ns(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

static void* worker(void* arg) {
  long id = (long)arg;
  long long start = now_ns();
  for (long i = 0; i < calls; i++) {
    syscall(SYS_getppid);
  }
  took_ns[id] = now_ns() - start;
  return NULL;
}

int main(int argc, char** argv) {
  if (argc != 3) {
    fprintf(stderr, "usage: %s CALLS THREADS\n", argv[0]);
    return 2;
  }
  calls = atol(argv[1]);
  int threads = atoi(argv[2]);
  if (calls < 1 || threads < 1 || threads > MAX_THREADS) {
    fprintf(stderr, "CALLS must be positive and THREADS 1 to %d\n", MAX_THREADS);
    return 2;
  }
  pthread_t handles[MAX_THREADS];
  long long start = now_ns();
  for (long i = 0; i < threads; i++) {
    if (pthread_create(&handles[i], NULL, worker, (void*)i) != 0) {
      perror("pthread_create");
      return 1;
    }
  }
  for (int i = 0; i < threads; i++) {
    pthread_join(handles[i], NULL);
  }
  printf("total_ns=%lld", now_ns() - start);
  for (int i = 0; i < threads; i++) {
    printf(" t%d_ns=%lld", i, took_ns[i]);
  }
  printf("\n");
  return 0;
}
