/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

enum { ITERATIONS = 100000 };

int main(void) {
  uint64_t completed = 0;

  for (uint64_t iteration = 0; iteration < ITERATIONS; ++iteration) {
    long result;
    if ((iteration & 1U) == 0) {
      result = syscall(SYS_getpid);
    } else {
      struct timespec now;
      result = syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &now);
    }
    if (result < 0) {
      perror("syscall");
      return errno == 0 ? 1 : errno;
    }
    ++completed;
  }

  printf("%" PRIu64 "\n", completed);
  return 0;
}
