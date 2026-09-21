/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

enum { THREADS = 4, ROUNDS = 500 };

struct worker {
  unsigned id;
  uint64_t checksum;
  int error;
};

static void *run_worker(void *opaque) {
  struct worker *worker = opaque;
  sigset_t blocked;
  sigset_t previous;
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGUSR1);

  uint64_t value = worker->id + 1;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    int error = pthread_sigmask(SIG_BLOCK, &blocked, &previous);
    if (error == 0) {
      error = pthread_sigmask(SIG_SETMASK, &previous, NULL);
    }
    if (error != 0) {
      worker->error = error;
      return NULL;
    }
    value = value * 6364136223846793005ULL + 1442695040888963407ULL;
  }
  worker->checksum = value;
  return NULL;
}

int main(void) {
  pthread_t threads[THREADS];
  struct worker workers[THREADS] = {0};
  uint64_t checksum = 0;

  for (unsigned index = 0; index < THREADS; ++index) {
    workers[index].id = index;
    if (pthread_create(&threads[index], NULL, run_worker, &workers[index]) != 0) {
      return 10;
    }
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    if (pthread_join(threads[index], NULL) != 0 || workers[index].error != 0) {
      fprintf(stderr, "worker=%u error=%d\n", index, workers[index].error);
      return 20;
    }
    checksum ^= workers[index].checksum;
  }

  printf("sigmask-stress threads=%d rounds=%d checksum=%016lx\n", THREADS,
         ROUNDS, (unsigned long)checksum);
  return EXIT_SUCCESS;
}
