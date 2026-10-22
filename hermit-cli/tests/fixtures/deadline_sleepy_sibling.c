/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// https://github.com/rrnewton/hermit/issues/4121: the cost of the cap's turns.
// A makes argv[2] (thousands) getppid calls while sibling B loops nanosleep(argv[1] ns)
// until A is done. Prints A's CLOCK_MONOTONIC time and B's iteration count.
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
static atomic_int done; static long b_iters; static long ns;
static void *b_fn(void *arg) { (void)arg; struct timespec ts = {0, ns};
  while (!atomic_load(&done)) { nanosleep(&ts, NULL); b_iters++; } return NULL; }
int main(int argc, char **argv) {
  ns = argc > 1 ? atol(argv[1]) : 1000; long calls = (argc > 2 ? atol(argv[2]) : 100) * 1000L;
  pthread_t b; pthread_create(&b, NULL, b_fn, NULL);
  struct timespec t0, t1; clock_gettime(CLOCK_MONOTONIC, &t0);
  for (long i = 0; i < calls; i++) syscall(SYS_getppid);
  clock_gettime(CLOCK_MONOTONIC, &t1); atomic_store(&done, 1); pthread_join(b, NULL);
  double ms = (t1.tv_sec - t0.tv_sec) * 1e3 + (t1.tv_nsec - t0.tv_nsec) / 1e6;
  printf("sleep_ns=%ld calls=%ld a_virtual_ms=%.3f b_iters=%ld\n", ns, calls, ms, b_iters);
  return 0; }
