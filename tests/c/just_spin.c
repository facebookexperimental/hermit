/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <pthread.h>
#include <stdlib.h>

static long x = 0;

void __attribute__((noinline)) foo() {
  x *= 2;
  x += 10;
  x /= 2;
}
void __attribute__((optnone)) spin() {
  for (int i = 0; i < 15 * 1000 * 1000; i++) {
    foo();
  }
}

void* thread(void* vargp) {
  spin();
  return NULL;
}

int main() {
  pthread_t thread_id;
  pthread_create(&thread_id, NULL, thread, NULL);
  spin();
  pthread_join(thread_id, NULL);
  return 0;
}
