/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Two threads compute in chunks and read the monotonic clock after each
 * chunk; each prints a checksum and its last reading. Under --chaos their
 * slices end at preemption points, so a preemption record from one run, with
 * most points removed, replays slices far longer than --max-timeslice.
 *
 * usage: uncapped_slice_replay [chunks]
 */

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static int chunks = 40;

struct result {
  uint64_t sum;
  long long last;
};

static void* work(void* arg) {
  struct result* result = arg;
  uint64_t x = (uint64_t)(uintptr_t)arg | 1;
  for (int i = 0; i < chunks; i++) {
    for (int j = 0; j < 20000; j++) {
      x = x * 6364136223846793005ULL + 1442695040888963407ULL;
      result->sum += x >> 33;
    }
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    result->last = (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
  }
  return NULL;
}

int main(int argc, char** argv) {
  if (argc > 1) {
    chunks = atoi(argv[1]);
  }
  struct result a = {0, 0}, b = {0, 0};
  pthread_t thread;
  if (pthread_create(&thread, NULL, work, &b) != 0) {
    perror("pthread_create");
    return 1;
  }
  work(&a);
  pthread_join(thread, NULL);
  printf("a sum=%llu last=%lld\n", (unsigned long long)a.sum, a.last);
  printf("b sum=%llu last=%lld\n", (unsigned long long)b.sum, b.last);
  return 0;
}
