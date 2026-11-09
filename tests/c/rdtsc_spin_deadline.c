/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Threads that spin on the TSC until a deadline, then print how many times
 * each one read it. A read that returns a later time ends a spin earlier, so
 * the counts change if any read returns a different value.
 */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>
#include <x86intrin.h>

#define SPINNERS 2
#define ROUNDS 8
#define SPIN_CYCLES 20000

static long reads[SPINNERS];

static void* spin(void* arg) {
  long id = (long)arg;
  for (int round = 0; round < ROUNDS; round++) {
    uint64_t deadline = __rdtsc() + SPIN_CYCLES;
    while (__rdtsc() < deadline) {
      reads[id]++;
    }
    getppid();
  }
  return NULL;
}

int main(void) {
  pthread_t threads[SPINNERS];
  for (long i = 0; i < SPINNERS; i++) {
    if (pthread_create(&threads[i], NULL, spin, (void*)i) != 0) {
      perror("pthread_create");
      return 1;
    }
  }
  for (int i = 0; i < SPINNERS; i++) {
    pthread_join(threads[i], NULL);
  }
  for (int i = 0; i < SPINNERS; i++) {
    printf("spinner %d: %ld reads\n", i, reads[i]);
  }
  return 0;
}
