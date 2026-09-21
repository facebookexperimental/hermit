/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <cpuid.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/random.h>
#include <sys/stat.h>
#include <time.h>

enum { THREADS = 2, RANDOM_BYTES = 8 };

struct observation {
  struct timespec realtime;
  struct timespec monotonic;
  struct stat executable;
  unsigned int cpuid_max_leaf;
  unsigned char random[RANDOM_BYTES];
  int completion_order;
  int error;
};

static atomic_int next_completion;

static void *observe(void *opaque) {
  struct observation *result = opaque;
  unsigned int unused_b;
  unsigned int unused_c;
  unsigned int unused_d;

  if (clock_gettime(CLOCK_REALTIME, &result->realtime) != 0 ||
      clock_gettime(CLOCK_MONOTONIC, &result->monotonic) != 0 ||
      stat("/proc/self/exe", &result->executable) != 0 ||
      getrandom(result->random, sizeof(result->random), 0) !=
          (ssize_t)sizeof(result->random)) {
    result->error = 1;
    return NULL;
  }

  if (__get_cpuid(0, &result->cpuid_max_leaf, &unused_b, &unused_c,
                  &unused_d) == 0) {
    result->error = 1;
    return NULL;
  }

  result->completion_order = atomic_fetch_add(&next_completion, 1);
  return NULL;
}

static void print_random(const unsigned char random[RANDOM_BYTES]) {
  for (int byte = 0; byte < RANDOM_BYTES; byte++) {
    printf("%02x", random[byte]);
  }
}

int main(void) {
  pthread_t threads[THREADS];
  struct observation observations[THREADS];
  memset(observations, 0, sizeof(observations));
  atomic_init(&next_completion, 0);

  for (int thread = 0; thread < THREADS; thread++) {
    if (pthread_create(&threads[thread], NULL, observe,
                       &observations[thread]) != 0) {
      return 2;
    }
  }
  for (int thread = 0; thread < THREADS; thread++) {
    if (pthread_join(threads[thread], NULL) != 0 ||
        observations[thread].error != 0) {
      return 3;
    }
  }

  puts("flag-matrix-probe");
  for (int thread = 0; thread < THREADS; thread++) {
    const struct observation *observation = &observations[thread];
    printf(
        "thread=%d order=%d realtime=%lld.%09ld monotonic=%lld.%09ld "
        "mtime=%lld.%09ld cpuid=%u random=",
        thread, observation->completion_order,
        (long long)observation->realtime.tv_sec, observation->realtime.tv_nsec,
        (long long)observation->monotonic.tv_sec,
        observation->monotonic.tv_nsec,
        (long long)observation->executable.st_mtim.tv_sec,
        observation->executable.st_mtim.tv_nsec, observation->cpuid_max_leaf);
    print_random(observation->random);
    putchar('\n');
  }
  return 0;
}
