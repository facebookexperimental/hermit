/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// aarch64: threads add to one counter through a load/store-exclusive retry
// loop (LDAXR, ADD, STLXR, CBNZ), the shape `--rewrite-exclusives` replaces
// with one LSE atomic. Prints the final count, which must be exact whether
// or not the loop was rewritten, and exits 0 only then.

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>

#define THREADS 4
#define INCREMENTS 10000

static uint32_t counter;

// Not inlined, so the loop is in the image exactly once whatever the
// compiler unrolls, and aligned to 16 bytes, so its four instructions never
// straddle a page (where the rewrite leaves a sequence by design).
__attribute__((noinline)) static void add_one(uint32_t* target) {
  uint32_t value;
  uint32_t failed;
  __asm__ volatile(
      "   .balign 16\n"
      "1: ldaxr %w0, [%2]\n"
      "   add %w0, %w0, #1\n"
      "   stlxr %w1, %w0, [%2]\n"
      "   cbnz %w1, 1b\n"
      : "=&r"(value), "=&r"(failed)
      : "r"(target)
      : "memory");
}

static void* add_many(void* unused) {
  (void)unused;
  for (int i = 0; i < INCREMENTS; i++) {
    add_one(&counter);
  }
  return NULL;
}

int main(void) {
  pthread_t threads[THREADS];
  for (int i = 0; i < THREADS; i++) {
    if (pthread_create(&threads[i], NULL, add_many, NULL) != 0) {
      return 2;
    }
  }
  for (int i = 0; i < THREADS; i++) {
    pthread_join(threads[i], NULL);
  }
  printf("counter %u\n", counter);
  return counter == THREADS * INCREMENTS ? 0 : 1;
}
