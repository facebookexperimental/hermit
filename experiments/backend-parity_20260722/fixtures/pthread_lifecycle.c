/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <pthread.h>
#include <stdio.h>

enum { THREADS = 4 };

static void* thread_main(void* argument) {
  int* value = argument;
  *value += 1;
  return NULL;
}

int main(void) {
  pthread_t threads[THREADS];
  int values[THREADS] = {0, 1, 2, 3};
  int total = 0;

  for (int index = 0; index < THREADS; ++index) {
    if (pthread_create(&threads[index], NULL, thread_main, &values[index]) != 0) {
      return 1;
    }
  }
  for (int index = 0; index < THREADS; ++index) {
    if (pthread_join(threads[index], NULL) != 0) {
      return 2;
    }
    total += values[index];
  }

  printf("threads=%d total=%d\n", THREADS, total);
  return 0;
}
