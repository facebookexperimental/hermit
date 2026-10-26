/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A data race whose window holds no conditional branch.
//
// Two threads each add 1 to a shared counter ITERATIONS times (argv[1],
// default 100000), after a start barrier, with a plain load, add and store:
// on x86-64 exactly `mov (counter),%rax; add $1,%rax; mov %rax,(counter)`, so
// no compiler can put a branch, a lock prefix or a fence inside. A thread that
// is preempted between its load and its store and then stores a stale value
// loses the other thread's updates. It prints
// "counter=<C> expected=<E> lost=<E-C>" and exits 0 either way.
//
// Natively the threads run on two CPUs and lose updates on nearly every run.
// Under `hermit run --chaos` a preemption lands only right after a retired
// conditional branch: the loop's own branch, which leaves the thread at its
// load, before the window. So no seed can lose an update, until
// `--chaos-instruction-offset` lets a preemption land some instructions later.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

static long counter;
static long iterations = 100000;
static pthread_barrier_t start;

static void increment(void) {
#if defined(__x86_64__)
  __asm__ volatile(
      "movq %0, %%rax\n\t"
      "addq $1, %%rax\n\t"
      "movq %%rax, %0"
      : "+m"(counter)
      :
      : "rax", "memory");
#else
  // A load, an add and a store, with no branch between them.
  *(volatile long*)&counter = *(volatile long*)&counter + 1;
#endif
}

static void* worker(void* arg) {
  (void)arg;
  pthread_barrier_wait(&start);
  for (long i = 0; i < iterations; i++) {
    increment();
  }
  return NULL;
}

int main(int argc, char** argv) {
  if (argc > 1) {
    iterations = strtol(argv[1], NULL, 10);
    if (iterations <= 0) {
      fprintf(stderr, "usage: %s [iterations > 0]\n", argv[0]);
      return 64;
    }
  }
  pthread_t threads[2];
  pthread_barrier_init(&start, NULL, 2);
  for (int i = 0; i < 2; i++) {
    if (pthread_create(&threads[i], NULL, worker, NULL) != 0) {
      perror("pthread_create");
      return 1;
    }
  }
  for (int i = 0; i < 2; i++) {
    pthread_join(threads[i], NULL);
  }
  long expected = 2 * iterations;
  printf("counter=%ld expected=%ld lost=%ld\n", counter, expected,
         expected - counter);
  return 0;
}
