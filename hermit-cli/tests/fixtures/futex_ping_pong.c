/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Two threads hand one futex word back and forth: each waits until the word
// names it, then names the other and wakes it. Every wait that blocks is
// ended by the other thread's wake, so the wakes alternate direction.

#include <linux/futex.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

#define ROUNDS 10

static _Atomic int turn;

static void wait_for(int want) {
  int seen;
  while ((seen = atomic_load(&turn)) != want) {
    syscall(SYS_futex, &turn, FUTEX_WAIT_PRIVATE, seen, NULL, NULL, 0);
  }
}

static void hand_to(int next) {
  atomic_store(&turn, next);
  syscall(SYS_futex, &turn, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
}

static void* worker(void* unused) {
  (void)unused;
  for (int round = 0; round < ROUNDS; round++) {
    wait_for(1);
    hand_to(0);
  }
  return NULL;
}

int main(void) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL) != 0) {
    return 2;
  }
  for (int round = 0; round < ROUNDS; round++) {
    hand_to(1);
    wait_for(0);
  }
  if (pthread_join(thread, NULL) != 0) {
    return 3;
  }
  printf("%d rounds\n", ROUNDS);
  return 0;
}
