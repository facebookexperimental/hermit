/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

// Though one thread does a lot of work before checking, there's no enforcement
// that global_str is actually set to a non-null value before use.

#define DO_WORK                     \
  do {                              \
    volatile int _work_var = 10000; \
    while (_work_var > 0) {         \
      _work_var--;                  \
    }                               \
  } while (0);

char* global_str = NULL;

void* Thread1(void* x) {
  DO_WORK
  if (!global_str) {
    // Simulate SEGFAULT, but exit cleanly because hermit doesn't handle this
    // well right now
    printf("ERROR! global_str is null at use.\n");
    exit(1);
  }
  printf("%s\n", global_str);
  return NULL;
}

void* Thread2(void* x) {
  global_str = "Hello world!";
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
