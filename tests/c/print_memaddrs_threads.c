/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// The multi-threaded companion of print_memaddrs.c: four threads, all alive
// at once, each allocate from the heap and record the addresses of their stack,
// their thread-local storage and their allocations. Main prints them in thread
// order once every thread has finished.
//
// The threads allocate one at a time, in a fixed order, so the addresses depend
// only on the memory layout, not on how the scheduler interleaves the threads:
// glibc gives each thread its own malloc arena the first time it allocates, and
// the order of those first allocations decides where each arena lands.

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#define THREADS 4

static __thread int thread_local_value;

struct record {
  uintptr_t stack; // this thread's stack, which is gone when main prints it
  void* tls;
  void* small;
  uintptr_t medium; // freed before main prints it
  void* large;
  void* after_free;
};

static struct record records[THREADS];
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t turn_changed = PTHREAD_COND_INITIALIZER;
static int turn;
static int started;

static void* allocate(void* argument) {
  int index = (int)(long)argument;
  int local = index;
  pthread_mutex_lock(&lock);
  started++;
  pthread_cond_broadcast(&turn_changed);
  // Every thread exists before the first one allocates.
  while (turn != index || started != THREADS) {
    pthread_cond_wait(&turn_changed, &lock);
  }
  struct record* record = &records[index];
  record->stack = (uintptr_t)&local;
  record->tls = &thread_local_value;
  record->small = malloc(100);
  void* medium = malloc(10000);
  record->medium = (uintptr_t)medium;
  record->large = malloc(1000000);
  free(medium);
  record->after_free = malloc(10000);
  turn++;
  pthread_cond_broadcast(&turn_changed);
  pthread_mutex_unlock(&lock);
  return NULL;
}

int main(void) {
  int local = 0;
  void* main_small = malloc(100);
  printf("main: stack %p thread-local %p malloc %p\n",
         (void*)&local,
         (void*)&thread_local_value,
         main_small);

  pthread_t threads[THREADS];
  for (long i = 0; i < THREADS; i++) {
    if (pthread_create(&threads[i], NULL, allocate, (void*)i) != 0) {
      perror("pthread_create");
      return 1;
    }
  }
  for (int i = 0; i < THREADS; i++) {
    pthread_join(threads[i], NULL);
  }

  for (int i = 0; i < THREADS; i++) {
    struct record* record = &records[i];
    printf(
        "thread %d: stack %#lx thread-local %p malloc %p %#lx %p after free %p\n",
        i,
        (unsigned long)record->stack,
        record->tls,
        record->small,
        (unsigned long)record->medium,
        record->large,
        record->after_free);
  }
  void* main_after = malloc(100);
  printf("main after threads: malloc %p\n", main_after);
  return 0;
}
