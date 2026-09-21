/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <pthread.h>
#include <stdio.h>
#include <stdatomic.h>
#include <stdlib.h>

// Though one thread does a lot of work before checking, there's no enforcement
// that global_str is actually set to a non-null value before use.

#define DO_WORK                     \
  do {                              \
    volatile int _work_var = 10000; \
    while (_work_var > 0) {         \
      _work_var--;                  \
    }                               \
  } while (0);

_Atomic(char*) global_str = NULL;

void* Thread1(void* x) {
  DO_WORK
  char* observed = atomic_load_explicit(&global_str, memory_order_relaxed);
  if (!observed) {
    // Simulate SEGFAULT, but exit cleanly because hermit doesn't handle this
    // well right now
    printf("ERROR! global_str is null at use.\n");
    exit(1);
  }
  printf("%s\n", observed);
  return NULL;
}

void* Thread2(void* x) {
  atomic_store_explicit(&global_str, "Hello world!", memory_order_relaxed);
  return NULL;
}

int main() {
  pthread_t t[2];
  pthread_create(&t[1], NULL, Thread2, NULL);
  pthread_create(&t[0], NULL, Thread1, NULL);
  pthread_join(t[1], NULL);
  pthread_join(t[0], NULL);
  return 0;
}
