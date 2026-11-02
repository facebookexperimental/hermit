/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A guest whose output is its thread schedule, for the record/replay chaos
 * test in hermit-cli/tests/record_replay.rs. Four threads each append their
 * letter to a shared buffer eight times, yielding after each append, and the
 * main thread prints the buffer once all have finished. Linux allows any
 * interleaving, so a recording made under a chaos seed prints that seed's
 * interleaving, and its replay must print the same.
 *
 * Usage: record_replay_chaos_race
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>

#define THREADS 4
#define ROUNDS 8

static char order[THREADS * ROUNDS + 1];
static atomic_int next;

static void* append(void* arg) {
  char letter = (char)('a' + (long)arg);
  for (int i = 0; i < ROUNDS; i++) {
    order[atomic_fetch_add(&next, 1)] = letter;
    sched_yield();
  }
  return NULL;
}

int main(void) {
  pthread_t threads[THREADS];
  for (long i = 0; i < THREADS; i++) {
    if (pthread_create(&threads[i], NULL, append, (void*)i) != 0) {
      perror("pthread_create");
      return 1;
    }
  }
  for (int i = 0; i < THREADS; i++) {
    pthread_join(threads[i], NULL);
  }
  printf("%s\n", order);
  return 0;
}
